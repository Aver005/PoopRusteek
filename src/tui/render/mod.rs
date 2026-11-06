//! Top-level frame assembly: routes to the chat view (one skeleton for the
//! welcome screen and the transcript), the onboarding screen, or the
//! management views, plus modals.
//!
//! Layout of this module tree, bottom layer first:
//! - [`util`], [`segments`], [`layout`] — pure text/format/layout helpers
//!   (no drawing); `layout::Regions` is the only place heights are computed
//! - [`popup`] — the shared popup/modal skeleton every modal builds on
//! - [`dock`], [`composer`], [`footer`], [`welcome`], [`onboarding`], [`mcp`],
//!   [`modals`] — the actual views; [`draw_frame`] below only dispatches.

mod approval;
mod attach;
mod chrome;
mod composer;
mod dock;
mod footer;
mod layout;
mod mcp;
mod modals;
mod onboarding;
mod popup;
mod providers;
mod search;
mod segments;
mod themes;
mod util;
mod welcome;

pub(crate) use approval::{ApprovalShape, viewport as approval_viewport};
pub(crate) use layout::{MAX_INPUT_ROWS, SIDEBAR_AUTO_COLS, SIDEBAR_WIDTH};
#[cfg(test)]
pub(crate) use util::fix_clock;

use crate::app::AppState;
use crate::app::events::View;
use crate::config::Config;
use crate::tui::TuiTerminal;
use crate::tui::theme::Theme;
use crate::tui::widgets;
use layout::{Needs, Regions};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Block;
use std::cell::RefCell;

const BRAND_TITLE: &str = "PoopRusteek \u{1F9FB}";

/// Меньше этого интерфейс не рисуется: рамки и ввод разъезжаются.
const MIN_COLS: u16 = 40;
const MIN_ROWS: u16 = 10;

/// Левое поле переписки и самая широкая строка текста: длиннее читать трудно.
const TRANSCRIPT_LEFT_MARGIN: u16 = 1;
const TRANSCRIPT_RIGHT_MARGIN: u16 = 1;
const TRANSCRIPT_MAX_WIDTH: u16 = 110;

thread_local! {
    /// Last title written to the terminal — OSC title changes are emitted
    /// only on change, not on every frame.
    static LAST_WINDOW_TITLE: RefCell<String> = const { RefCell::new(String::new()) };
}

/// The window title for the current app state: the brand on the landing /
/// onboarding / management screens, a truncated chat title (from the
/// chat's first real message) once a conversation has content.
fn window_title(state: &AppState) -> String {
    if state.view != View::Chat {
        return BRAND_TITLE.to_string();
    }
    let title = state
        .focused()
        .messages
        .iter()
        .find(|message| {
            !message.ui_only
                && matches!(
                    message.role,
                    crate::provider::Role::User | crate::provider::Role::Assistant
                )
                && !message.visible_content().trim().is_empty()
        })
        .map(|message| {
            let first_line = message
                .visible_content()
                .trim()
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            util::truncate(&first_line, 40)
        });
    match title {
        Some(title) if !title.is_empty() => title,
        _ => BRAND_TITLE.to_string(),
    }
}

fn sync_window_title(terminal: &mut TuiTerminal, state: &AppState) {
    let title = window_title(state);
    LAST_WINDOW_TITLE.with(|last| {
        let mut last = last.borrow_mut();
        if *last != title {
            let _ = crossterm::execute!(
                terminal.backend_mut(),
                crossterm::terminal::SetTitle(&title)
            );
            *last = title;
        }
    });
}

/// Тема кадра: из конфига, а на экране `/themes` — подсвеченная в мастере.
fn frame_theme(state: &AppState, config: &Config) -> Theme {
    // The active theme comes from config; while the `/themes` screen is
    // open the *whole frame* is drawn with the highlighted / in-progress
    // theme instead — that's the live preview, and leaving without applying
    // restores the saved theme on the next frame for free.
    if state.view == View::Themes {
        state.themes.preview_theme(config)
    } else {
        Theme::resolve(&config.ui)
    }
}

/// Главный экран без сообщений: на месте переписки рисуется приветствие.
fn is_welcome(state: &AppState) -> bool {
    state.focused().messages.is_empty() && !state.focused().generation.active
}

/// Чего экран хочет от раскладки.
fn needs_for(state: &AppState, area: Rect) -> Needs {
    Needs {
        dock_rows: dock::height(state),
        input_rows: widgets::input::visible_rows(&state.input.buffer, area.width),
        attachments: !state.attached_files.is_empty(),
        sidebar: !is_welcome(state) && state.stats_panel_visible(area.width),
    }
}

/// Раскладка экрана чата. Одна и та же для приветствия и переписки.
fn chat_regions(state: &AppState, area: Rect) -> Regions {
    Regions::compute(area, &needs_for(state, area))
}

/// Высота области переписки в текущем размере окна. `None`, пока размер не
/// известен или окно слишком мало для интерфейса.
pub(crate) fn transcript_rows(state: &AppState) -> Option<u16> {
    let (cols, rows) = (state.terminal_cols, state.terminal_rows);
    if cols < MIN_COLS || rows < MIN_ROWS || state.view != View::Chat {
        return None;
    }
    Some(
        chat_regions(state, Rect::new(0, 0, cols, rows))
            .transcript
            .height,
    )
}

/// Новая прокрутка (строки от низа), когда переписка стала `new` строк вместо
/// `old`: верхняя строка остаётся той же. У самого низа (0) ничего не меняется —
/// он приклеен. Ноль в `old` значит «прошлого кадра не было».
pub(crate) fn scroll_after_height_change(offset: u32, old: u16, new: u16) -> u32 {
    if offset == 0 || old == 0 {
        return offset;
    }
    (i64::from(offset) + i64::from(old) - i64::from(new)).max(0) as u32
}

/// Область текста внутри переписки: слева поле, ширина ограничена.
fn transcript_text_area(transcript: Rect) -> Rect {
    let x = transcript.x + TRANSCRIPT_LEFT_MARGIN.min(transcript.width);
    let room = transcript
        .width
        .saturating_sub(TRANSCRIPT_LEFT_MARGIN + TRANSCRIPT_RIGHT_MARGIN);
    Rect::new(
        x,
        transcript.y,
        room.min(TRANSCRIPT_MAX_WIDTH),
        transcript.height,
    )
}

/// Экран чата: приветствие или переписка сверху, снизу всегда одно и то же.
fn draw_chat_screen(
    frame: &mut ratatui::Frame,
    area: Rect,
    state: &AppState,
    config: &Config,
    theme: &Theme,
) -> Option<(u16, u16)> {
    let regions = chat_regions(state, area);
    if is_welcome(state) {
        welcome::render_welcome(frame, regions.transcript, state, config, theme);
    } else {
        widgets::chat::render_chat(
            frame,
            transcript_text_area(regions.transcript),
            state,
            theme,
        );
        if let Some(sidebar) = regions.sidebar {
            // Панель сама рисуется у правого края области, зазор слева — её разделитель.
            let strip = Rect::new(
                regions.transcript.x,
                sidebar.y,
                sidebar.right() - regions.transcript.x,
                sidebar.height,
            );
            widgets::panel::render_stats_panel(frame, strip, state, config, theme);
        }
    }
    attach::render_attach_bar(frame, regions.attach, state, theme);
    dock::render_dock(frame, regions.dock, state, theme);
    composer::render_rule(frame, regions.rule, state, theme);
    let cursor = widgets::input::render_input(frame, regions.input, state, theme);
    footer::render_footer(
        frame,
        regions.footer,
        state,
        config,
        theme,
        regions.dock.height > 0,
    );
    if state.autocomplete.visible {
        modals::render_autocomplete(frame, regions.rule, state, theme);
    }
    cursor
}

/// Один кадр целиком; вернёт, куда поставить курсор. Отдельно от [`render`],
/// чтобы тот же код рисовал в `TestBackend` (`tui::capture`).
pub(crate) fn draw_frame(
    frame: &mut ratatui::Frame,
    state: &AppState,
    config: &Config,
) -> Option<(u16, u16)> {
    crate::tui::glyphs::select(config.ui.glyphs);
    let theme = frame_theme(state, config);
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), area);
    if area.width < MIN_COLS || area.height < MIN_ROWS {
        render_too_small(frame, area, &theme);
        return None;
    }

    let mut cursor = None;
    if state.view == View::Onboarding {
        cursor = onboarding::render_onboarding(
            frame,
            area,
            &state.onboarding,
            state.focused().generation.animation_tick,
            &theme,
        );
        // Bottom status line — /logout and /wipe land here with their result.
        if area.height > 1 {
            let status_area = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
            footer::render_status_line(frame, status_area, state, &theme);
        }
    } else if state.view == View::Mcp {
        mcp::render_mcp_view(frame, area, &state.mcp_status.view, &theme);
    } else if state.view == View::Providers {
        providers::render_providers_view(frame, area, &state.providers_view, config, &theme);
    } else if state.view == View::Search {
        search::render_search_view(frame, area, &state.search, &theme);
    } else if state.view == View::Themes {
        themes::render_themes_view(frame, area, &state.themes, config, &theme);
    } else {
        cursor = draw_chat_screen(frame, area, state, config, &theme);
    }

    if let Some(modal) = &state.modal {
        popup::dim_backdrop(frame, area, &theme);
        modals::render_modal(frame, area, modal, &theme);
        cursor = None;
    }
    // Единственное место, где ставится курсор: ratatui сам показывает его
    // после кадра и прячет, если позиции нет.
    if let Some(position) = cursor {
        frame.set_cursor_position(position);
    }
    cursor
}

/// Окно слишком мало для интерфейса: вместо развалившейся картинки — подсказка.
fn render_too_small(frame: &mut ratatui::Frame, area: Rect, theme: &Theme) {
    let text = format!(
        "Window too small: {}x{}. Need at least {MIN_COLS}x{MIN_ROWS}.",
        area.width, area.height
    );
    let top = area.y + area.height.saturating_sub(1) / 2;
    frame.render_widget(
        ratatui::widgets::Paragraph::new(text)
            .alignment(ratatui::layout::Alignment::Center)
            .wrap(ratatui::widgets::Wrap { trim: true })
            .style(Style::default().fg(theme.warning).bg(theme.bg)),
        Rect::new(
            area.x,
            top,
            area.width,
            area.height.saturating_sub(top - area.y).min(3),
        ),
    );
}

pub fn render(
    terminal: &mut TuiTerminal,
    state: &AppState,
    config: &Config,
) -> crate::error::AppResult<()> {
    sync_window_title(terminal, state);
    terminal.draw(|frame| {
        draw_frame(frame, state, config);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::conversation::Conversation;
    use crate::provider::ChatMessage;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const SIZES: [(u16, u16); 9] = [
        (60, 20),
        (60, 40),
        (80, 24),
        (100, 30),
        (120, 40),
        (140, 40),
        (160, 50),
        (200, 60),
        (200, 30),
    ];

    fn state_for(welcome: bool, cols: u16, rows: u16) -> AppState {
        let mut state = AppState::new(Conversation::fresh_main(None));
        state.terminal_rows = rows;
        state.terminal_cols = cols;
        if !welcome {
            let messages = &mut state.focused_mut().messages;
            messages.push(ChatMessage::user("hello"));
            messages.push(ChatMessage::assistant("hi there"));
        }
        state
    }

    fn row_text(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect()
    }

    /// Номер строки, где нарисован знак приглашения ввода.
    fn prompt_row(terminal: &Terminal<TestBackend>) -> Option<u16> {
        let prompt = crate::tui::glyphs::get().prompt;
        (0..terminal.backend().buffer().area.height)
            .rev()
            .find(|y| row_text(terminal, *y).starts_with(&format!(" {prompt} ")))
    }

    fn config_with_unicode() -> Config {
        let mut config = Config::default();
        config.ui.glyphs = crate::config::GlyphMode::Unicode;
        config
    }

    /// Поле ввода стоит на одной и той же строке от низа на обоих экранах и
    /// на любом размере окна: вторая снизу, над подвалом.
    #[test]
    fn the_input_sits_on_the_same_row_from_the_bottom_on_every_screen_and_size() {
        let config = config_with_unicode();
        for (cols, rows) in SIZES {
            for welcome in [true, false] {
                let state = state_for(welcome, cols, rows);
                let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
                terminal
                    .draw(|frame| {
                        draw_frame(frame, &state, &config);
                    })
                    .unwrap();
                assert_eq!(
                    prompt_row(&terminal),
                    Some(rows - 2),
                    "{cols}x{rows} welcome={welcome}"
                );
            }
        }
    }

    #[test]
    fn the_regions_agree_with_the_drawn_input_row() {
        for (cols, rows) in SIZES {
            for welcome in [true, false] {
                let state = state_for(welcome, cols, rows);
                let regions = chat_regions(&state, Rect::new(0, 0, cols, rows));
                assert_eq!(regions.input.y, rows - 2, "{cols}x{rows}");
                assert_eq!(regions.footer.y, rows - 1, "{cols}x{rows}");
                assert_eq!(regions.input.bottom() + regions.footer.height, rows);
            }
        }
    }

    #[test]
    fn a_running_turn_opens_the_dock_without_moving_the_input() {
        let config = config_with_unicode();
        let (cols, rows) = (100, 30);
        let mut state = state_for(false, cols, rows);
        state.focused_mut().generation.active = true;
        let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        terminal
            .draw(|frame| {
                draw_frame(frame, &state, &config);
            })
            .unwrap();
        assert_eq!(prompt_row(&terminal), Some(rows - 2));
        let regions = chat_regions(&state, Rect::new(0, 0, cols, rows));
        assert_eq!(regions.dock.height, 1);
    }

    #[test]
    fn a_window_below_the_minimum_gets_a_hint_not_a_broken_layout() {
        let config = config_with_unicode();
        let state = state_for(true, 30, 8);
        let mut terminal = Terminal::new(TestBackend::new(30, 8)).unwrap();
        terminal
            .draw(|frame| {
                assert_eq!(draw_frame(frame, &state, &config), None);
            })
            .unwrap();
        let all: String = (0..8).map(|y| row_text(&terminal, y)).collect();
        assert!(all.contains("too small"), "{all}");
    }

    #[test]
    fn the_stats_panel_is_hidden_by_default_below_the_auto_width() {
        let mut state = state_for(false, 120, 40);
        assert!(
            chat_regions(&state, Rect::new(0, 0, 120, 40))
                .sidebar
                .is_none()
        );
        assert!(
            chat_regions(&state, Rect::new(0, 0, 160, 40))
                .sidebar
                .is_some()
        );
        // Явное решение человека важнее ширины.
        state.show_stats_panel = Some(false);
        assert!(
            chat_regions(&state, Rect::new(0, 0, 160, 40))
                .sidebar
                .is_none()
        );
        state.show_stats_panel = Some(true);
        assert!(
            chat_regions(&state, Rect::new(0, 0, 120, 40))
                .sidebar
                .is_some()
        );
    }

    #[test]
    fn the_scroll_follows_the_top_edge_when_the_transcript_changes_height() {
        // Док вырос на две строки: переписка стала ниже, прокрутка выросла на две.
        assert_eq!(scroll_after_height_change(10, 30, 28), 12);
        // Док исчез: переписка выше, прокрутка меньше, но не ниже нуля.
        assert_eq!(scroll_after_height_change(10, 28, 30), 8);
        assert_eq!(scroll_after_height_change(1, 28, 30), 0);
        // У низа — без изменений; без прошлого кадра — тоже.
        assert_eq!(scroll_after_height_change(0, 30, 28), 0);
        assert_eq!(scroll_after_height_change(7, 0, 28), 7);
    }

    #[test]
    fn the_transcript_rows_shrink_when_the_dock_opens() {
        let mut state = state_for(false, 100, 30);
        let idle = transcript_rows(&state).expect("size known");
        state.focused_mut().generation.active = true;
        let working = transcript_rows(&state).expect("size known");
        assert_eq!(idle - working, 1);
    }
}
