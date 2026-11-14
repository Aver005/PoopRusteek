//! Главная, пока в чате нет сообщений. Два вида (`[ui] landing`, `/landing`):
//! «center» — название и поле ввода в рамке по центру, под ним недавние сессии;
//! «sheet» — лист без рамок: вопрос, сессии фишками и строка ввода. Шапка общая с чатом.
//! Высоты главной считает этот модуль (`center_plan`, `SheetPlan`), чата — `layout`.

use super::composer::render_input_box;
use super::header::render_header;
use super::layout::{BOX_BORDERS, SIDE_MARGIN};
use super::util::{SHORT_DATE_COLS, fit_col, short_date, split_line, truncate};
use super::{attach, footer, modals};
use crate::app::AppState;
use crate::config::{Config, LandingStyle};
use crate::tui::glyphs;
use crate::tui::theme::Theme;
use crate::tui::widgets::input;
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

/// Название по центру и подпись под ним.
const LOGO: &str = "p o o p r u s t e e k";
const TAGLINE: &str = "coding agent in your terminal";
/// Шире поле на главной не бывает: длинную строку трудно охватить взглядом.
const BOX_MAX_COLS: u16 = 76;
/// Строк у названия: само название, подпись и пустая строка до поля.
const LOGO_ROWS: u16 = 3;
/// Перед сессиями две пустые строки и подпись.
const SESSIONS_LEAD_ROWS: u16 = 3;
/// ` ▸ 1 ` перед названием сессии.
const ROW_LEAD_COLS: usize = 5;
/// Подпись над сессиями и подсказка листа: как выбрать сессию с клавиатуры.
const PICK_HINT: &str = "\u{2193} pick \u{00B7} 1-5 or Enter open";
/// Отступ листа слева и справа; в узком окне — поле колонки.
const SHEET_INDENT: u16 = 8;
const SHEET_NARROW_COLS: u16 = 60;
/// Фишка сессии на листе: номер и не больше стольких колонок названия.
const CHIP_TITLE_COLS: usize = 18;
const CHIPS_LEAD: &str = "continue  ";
/// Вопрос и подсказка над вводом листа и пустая строка под ними.
const SHEET_HEAD_ROWS: u16 = 3;
/// Фишки и пустая строка под ними.
const SHEET_CHIP_ROWS: u16 = 2;

/// Сколько сессий главная нарисует в текущем окне: клавиши выбирают только их.
pub(super) fn visible_sessions(state: &AppState, config: &Config, area: Rect) -> usize {
    let Some(body) = body_of(area) else {
        return 0;
    };
    match config.ui.landing {
        LandingStyle::Center => CenterGeometry::new(body, state).plan.sessions,
        LandingStyle::Sheet => SheetPlan::new(body, state).chips,
    }
}

/// Тело главной: всё между шапкой и нижней строкой.
fn body_of(area: Rect) -> Option<Rect> {
    (area.height >= 3).then(|| Rect::new(area.x, area.y + 1, area.width, area.height - 2))
}

/// Нарисовать главную целиком: шапку, вид из конфига и нижнюю строку. Вернёт курсор.
pub(super) fn draw_welcome(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    config: &Config,
    theme: &Theme,
) -> Option<(u16, u16)> {
    let body = body_of(area)?;
    render_header(
        frame,
        Rect::new(area.x, area.y, area.width, 1),
        state,
        theme,
    );
    let bottom = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    match config.ui.landing {
        LandingStyle::Center => {
            footer::render_home_footer(frame, bottom, state, config, theme);
            draw_center(frame, body, state, config, theme)
        }
        LandingStyle::Sheet => {
            footer::render_sheet_footer(frame, bottom, state, config, theme, "/help");
            draw_sheet(frame, body, state, theme)
        }
    }
}

/// Что из «center» помещается в высоту: название, сколько сессий и отступ сверху.
#[derive(Debug, PartialEq, Eq)]
struct CenterPlan {
    logo: bool,
    sessions: usize,
    top: u16,
}

/// Поле с подписью под ним важнее всего, потом сессии, потом название.
fn center_plan(height: u16, input_rows: u16, attach: bool, sessions: usize) -> CenterPlan {
    let core = u16::from(attach) + input_rows + BOX_BORDERS + 1;
    let mut room = height.saturating_sub(core);
    let wanted = if sessions > 0 {
        SESSIONS_LEAD_ROWS + sessions as u16
    } else {
        0
    };
    let logo = room >= LOGO_ROWS + wanted;
    if logo {
        room -= LOGO_ROWS;
    }
    let sessions = if room > SESSIONS_LEAD_ROWS {
        sessions.min(usize::from(room - SESSIONS_LEAD_ROWS))
    } else {
        0
    };
    if sessions > 0 {
        room -= SESSIONS_LEAD_ROWS + sessions as u16;
    }
    // Чуть выше середины: внизу глазу нужно место под список.
    CenterPlan {
        logo,
        sessions,
        top: room * 2 / 5,
    }
}

/// Ширина поля, его высота и план «center» для тела `body`.
struct CenterGeometry {
    box_x: u16,
    box_w: u16,
    input_rows: u16,
    attach: bool,
    plan: CenterPlan,
}

impl CenterGeometry {
    fn new(body: Rect, state: &AppState) -> Self {
        let box_w = body
            .width
            .saturating_sub(2 * SIDE_MARGIN)
            .min(BOX_MAX_COLS)
            .max(body.width.min(4));
        let attach = !state.attached_files.is_empty();
        // В совсем низком окне поле не выходит за низ: рамка и подпись должны влезть.
        let max_input = body
            .height
            .saturating_sub(u16::from(attach) + BOX_BORDERS + 1)
            .max(1);
        let input_rows =
            input::visible_rows(&state.input.buffer, box_w.saturating_sub(2)).min(max_input);
        let plan = center_plan(
            body.height,
            input_rows,
            attach,
            state.landing.recent().len(),
        );
        Self {
            box_x: body.x + (body.width - box_w) / 2,
            box_w,
            input_rows,
            attach,
            plan,
        }
    }
}

fn draw_center(
    frame: &mut Frame,
    body: Rect,
    state: &AppState,
    config: &Config,
    theme: &Theme,
) -> Option<(u16, u16)> {
    let CenterGeometry {
        box_x,
        box_w,
        input_rows,
        attach,
        plan,
    } = CenterGeometry::new(body, state);
    let mut y = body.y + plan.top;
    let bg = Style::default().bg(theme.bg);
    let dim = bg.fg(theme.text_dim);

    if plan.logo {
        let logo = Line::from(Span::styled(
            LOGO,
            bg.fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
        let tagline = Line::from(Span::styled(TAGLINE, dim));
        frame.render_widget(
            Paragraph::new(vec![logo, tagline]).alignment(Alignment::Center),
            Rect::new(body.x, y, body.width, 2).intersection(body),
        );
        y += LOGO_ROWS;
    }
    if attach {
        let bar = Rect::new(box_x, y, box_w, 1).intersection(body);
        attach::render_attach_bar(frame, bar, state, theme);
        y += 1;
    }
    let input_box = Rect::new(box_x, y, box_w, input_rows + BOX_BORDERS).intersection(body);
    render_input_box(frame, input_box, state, theme);
    let inner = Rect::new(box_x + 1, y + 1, box_w.saturating_sub(2), input_rows).intersection(body);
    let cursor = input::render_input(frame, inner, state, theme);
    y = input_box.bottom();

    let model = footer::model_label(state, config).unwrap_or_default();
    let subline = split_line(
        vec![Span::styled(state.workspace_path.clone(), dim)],
        vec![Span::styled(format!("{model} \u{00B7} /help"), dim)],
        usize::from(box_w.saturating_sub(4)),
    );
    frame.render_widget(
        Paragraph::new(subline),
        Rect::new(box_x + 2, y, box_w.saturating_sub(4), 1).intersection(body),
    );
    y += 1;

    if plan.sessions > 0 {
        // Две пустые строки, подпись, затем сами сессии.
        let label_y = y + SESSIONS_LEAD_ROWS - 1;
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" recent  {PICK_HINT}"),
                dim,
            ))),
            Rect::new(box_x, label_y, box_w, 1).intersection(body),
        );
        let rows = session_rows(state, plan.sessions, usize::from(box_w), theme);
        frame.render_widget(
            Paragraph::new(rows),
            Rect::new(box_x, label_y + 1, box_w, plan.sessions as u16).intersection(body),
        );
    }
    if state.autocomplete.visible {
        modals::render_autocomplete(frame, input_box, state, theme);
    }
    cursor
}

/// Строки недавних сессий: `▸ 1  название ……… дата`, выбранная — с подсветкой.
fn session_rows(state: &AppState, rows: usize, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    // Пробел перед датой и пробел после неё.
    let title_w = width.saturating_sub(ROW_LEAD_COLS + 1 + SHORT_DATE_COLS + 1);
    let today = chrono::Local::now().date_naive();
    state
        .landing
        .recent()
        .iter()
        .take(rows)
        .enumerate()
        .map(|(index, session)| {
            let picked = state.landing.cursor() == Some(index);
            let bg = if picked { theme.selection } else { theme.bg };
            let marker = if picked { glyphs::get().pointer } else { " " };
            let title_color = if picked { theme.fg } else { theme.text_soft };
            Line::from(vec![
                Span::styled(
                    format!(" {marker} {} ", index + 1),
                    Style::default().fg(theme.accent).bg(bg),
                ),
                Span::styled(
                    fit_col(&session.title, title_w),
                    Style::default().fg(title_color).bg(bg),
                ),
                Span::styled(" ", Style::default().bg(bg)),
                Span::styled(
                    fit_col(
                        &format!(
                            "{:>SHORT_DATE_COLS$}",
                            short_date(&session.updated_at, today)
                        ),
                        SHORT_DATE_COLS,
                    ),
                    Style::default().fg(theme.text_dim).bg(bg),
                ),
                Span::styled(" ", Style::default().bg(bg)),
            ])
        })
        .collect()
}

/// Что из «sheet» помещается: отступ, ширина, высота ввода, сколько фишек и место сверху.
struct SheetPlan {
    x: u16,
    width: u16,
    input_rows: u16,
    attach: bool,
    /// Вопрос и подсказка над вводом; в низком окне их нет, как и фишек.
    head: bool,
    chips: usize,
    top: u16,
}

impl SheetPlan {
    fn new(body: Rect, state: &AppState) -> Self {
        let indent = if body.width >= SHEET_NARROW_COLS {
            SHEET_INDENT
        } else {
            SIDE_MARGIN.min(body.width / 4)
        };
        let width = body.width.saturating_sub(2 * indent);
        let attach = !state.attached_files.is_empty();
        let input_rows = input::visible_rows(&state.input.buffer, width)
            .min(body.height.saturating_sub(u16::from(attach)).max(1));
        let core = u16::from(attach) + input_rows;
        let fitting = chip_count(state, usize::from(width));
        let full = SHEET_HEAD_ROWS + if fitting > 0 { SHEET_CHIP_ROWS } else { 0 } + 1 + core;
        let head = body.height >= full;
        let chips = if head { fitting } else { 0 };
        let block = if head { full } else { core };
        Self {
            x: body.x + indent,
            width,
            input_rows,
            attach,
            head,
            chips,
            // Примерно на трети высоты.
            top: body.height.saturating_sub(block) / 3,
        }
    }
}

/// Текст фишки сессии номер `index`.
fn chip_text(index: usize, title: &str) -> String {
    format!(" {} {} ", index + 1, truncate(title, CHIP_TITLE_COLS))
}

/// Сколько фишек влезает в ширину `width` после подписи.
fn chip_count(state: &AppState, width: usize) -> usize {
    let mut used = CHIPS_LEAD.width();
    let mut count = 0;
    for (index, session) in state.landing.recent().iter().enumerate() {
        let cost = chip_text(index, &session.title).width() + 1;
        if used + cost > width {
            break;
        }
        used += cost;
        count += 1;
    }
    count
}

fn draw_sheet(
    frame: &mut Frame,
    body: Rect,
    state: &AppState,
    theme: &Theme,
) -> Option<(u16, u16)> {
    let plan = SheetPlan::new(body, state);
    let bg = Style::default().bg(theme.bg);
    let mut y = body.y + plan.top;
    if plan.head {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "What are we building?",
                    bg.fg(theme.fg).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    if plan.chips > 0 {
                        format!("Describe a task. / for commands, @ to attach a file, {PICK_HINT}.")
                    } else {
                        "Describe a task. / for commands, @ to attach a file.".to_string()
                    },
                    bg.fg(theme.text_dim),
                )),
            ]),
            Rect::new(plan.x, y, plan.width, 2).intersection(body),
        );
        y += SHEET_HEAD_ROWS;
        if plan.chips > 0 {
            frame.render_widget(
                Paragraph::new(chip_line(state, plan.chips, theme)),
                Rect::new(plan.x, y, plan.width, 1).intersection(body),
            );
            y += SHEET_CHIP_ROWS;
        }
        y += 1;
    }
    if plan.attach {
        let bar = Rect::new(plan.x, y, plan.width, 1).intersection(body);
        attach::render_attach_bar(frame, bar, state, theme);
        y += 1;
    }
    let field = Rect::new(plan.x, y, plan.width, plan.input_rows).intersection(body);
    let cursor = input::render_input(frame, field, state, theme);
    if state.autocomplete.visible {
        modals::render_autocomplete(frame, field, state, theme);
    }
    cursor
}

/// `continue  [1 items::range] [2 CI workflow] …` — первые `count` фишек.
fn chip_line(state: &AppState, count: usize, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::styled(
        CHIPS_LEAD,
        Style::default().fg(theme.text_dim),
    )];
    for (index, session) in state.landing.recent().iter().take(count).enumerate() {
        let picked = state.landing.cursor() == Some(index);
        let style = if picked {
            Style::default()
                .fg(theme.accent)
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_soft).bg(theme.selection)
        };
        spans.push(Span::styled(chip_text(index, &session.title), style));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::conversation::Conversation;
    use crate::session::SessionSummary;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn summary(title: &str) -> SessionSummary {
        SessionSummary {
            id: title.to_string(),
            created_at: String::new(),
            updated_at: "2026-11-05T09:12:00+00:00".to_string(),
            workspace_root: String::new(),
            message_count: 3,
            title: title.to_string(),
            model_type: String::new(),
            tag: None,
            broken: false,
        }
    }

    fn state_with(sessions: usize) -> AppState {
        let mut state = AppState::new(Conversation::fresh_main(None));
        state.landing.apply(
            (0..sessions)
                .map(|n| summary(&format!("session number {n}")))
                .collect(),
        );
        state
    }

    fn screen(state: &AppState, style: LandingStyle, cols: u16, rows: u16) -> String {
        let mut config = Config::default();
        config.ui.landing = style;
        let theme = Theme::default_dark();
        let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        terminal
            .draw(|frame| {
                draw_welcome(frame, frame.area(), state, &config, &theme);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..rows)
            .map(|y| {
                (0..cols)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_tall_window_fits_the_logo_and_every_session() {
        let plan = center_plan(30, 1, false, 5);
        assert!(plan.logo);
        assert_eq!(plan.sessions, 5);
    }

    #[test]
    fn sessions_go_before_the_logo() {
        let short = center_plan(10, 1, false, 5);
        assert!(!short.logo && short.sessions > 0, "{short:?}");
        let tiny = center_plan(4, 1, false, 5);
        assert_eq!((tiny.logo, tiny.sessions), (false, 0));
    }

    #[test]
    fn the_plan_never_asks_for_more_rows_than_it_has() {
        for height in 4..40u16 {
            for sessions in 0..6 {
                for input_rows in 1..=3 {
                    let plan = center_plan(height, input_rows, true, sessions);
                    let core = 1 + input_rows + BOX_BORDERS + 1;
                    let used = plan.top
                        + core
                        + if plan.logo { LOGO_ROWS } else { 0 }
                        + if plan.sessions > 0 {
                            SESSIONS_LEAD_ROWS + plan.sessions as u16
                        } else {
                            0
                        };
                    assert!(
                        used <= height.max(core),
                        "h {height} s {sessions} i {input_rows}: {plan:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_center_draws_the_input_and_exactly_the_sessions_it_counts() {
        let state = state_with(5);
        let config = Config::default();
        for (cols, rows) in [(40, 10), (80, 24), (120, 40)] {
            let text = screen(&state, LandingStyle::Center, cols, rows);
            let shown = visible_sessions(&state, &config, Rect::new(0, 0, cols, rows));
            assert!(text.contains(glyphs::get().prompt), "{cols}x{rows}\n{text}");
            for n in 0..5 {
                let drawn = text.contains(&format!("session number {n}"));
                assert_eq!(drawn, n < shown, "{cols}x{rows} session {n}\n{text}");
            }
        }
    }

    #[test]
    fn the_sheet_draws_exactly_the_chips_it_counts() {
        let state = state_with(5);
        let mut config = Config::default();
        config.ui.landing = LandingStyle::Sheet;
        for (cols, rows) in [(40, 10), (80, 24), (200, 40)] {
            let text = screen(&state, LandingStyle::Sheet, cols, rows);
            let shown = visible_sessions(&state, &config, Rect::new(0, 0, cols, rows));
            for n in 0..5 {
                let drawn = text.contains(&format!(" {} session", n + 1));
                assert_eq!(drawn, n < shown, "{cols}x{rows} chip {n}\n{text}");
            }
        }
        let wide = screen(&state, LandingStyle::Sheet, 120, 40);
        assert!(wide.contains("What are we building?"));
    }
}
