//! `/resume` — выбор сессии на весь экран. Шапка, подпись и фильтр стоят на
//! месте, а список прокручивается под ними; полоса справа показывает, где окно.

use super::header::render_header;
use super::session_row::{RowLayout, session_line};
use crate::app::AppState;
use crate::app::list::{ListWindow, NAV_HINT_ARROWS, page_rows, rows};
use crate::tui::glyphs;
use crate::tui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Над списком: шапка, пустая, подпись, пустая, фильтр, пустая. Вместе с
/// двумя строками снизу это служебные строки `list::page_rows`.
const TOP_ROWS: u16 = 6;
/// Поле слева и справа.
const MARGIN: u16 = 2;
/// С этой ширины показываем колонки модели и числа сообщений.
const WIDE_COLS: usize = 70;
/// Справа место под полосу прокрутки.
const SCROLLBAR_TRAIL: usize = 2;

/// Нарисовать экран; вернёт, где стоит курсор фильтра (ставит его `draw_frame`).
pub(super) fn render_resume_view(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    theme: &Theme,
) -> Option<(u16, u16)> {
    let workspace = state.workspace_path.as_str();
    let resume = &state.resume;
    let sessions = resume.visible(workspace);
    let bg = Style::default().bg(theme.bg);
    let dim = bg.fg(theme.text_dim);
    let x = area.x + MARGIN;
    let width = area.width.saturating_sub(2 * MARGIN);

    render_header(
        frame,
        Rect::new(area.x, area.y, area.width, 1),
        state,
        theme,
    );

    let scope = if resume.all_projects {
        "all projects".to_string()
    } else {
        folder_name(workspace)
    };
    let count = if resume.loading {
        "loading\u{2026}".to_string()
    } else if sessions.is_empty() {
        format!("0 of {} \u{00B7} {scope}", resume.scope_len(workspace))
    } else {
        format!(
            "{} of {} \u{00B7} {scope}",
            resume.cursor.selected + 1,
            sessions.len()
        )
    };
    let title = Line::from(vec![
        Span::styled(
            "Continue a session   ",
            bg.fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(count, dim),
    ]);
    frame.render_widget(Paragraph::new(title), Rect::new(x, area.y + 2, width, 1));

    let filter = if resume.filter.buffer.is_empty() {
        Span::styled("type to filter by title", dim)
    } else {
        Span::styled(resume.filter.buffer.clone(), bg.fg(theme.fg))
    };
    let prompt = format!("{} ", glyphs::get().prompt);
    let filter_y = area.y + 4;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prompt.clone(), bg.fg(theme.accent)),
            filter,
        ])),
        Rect::new(x, filter_y, width, 1),
    );

    // Та же высота, на которую листают клавиши.
    let list_h =
        (page_rows(area.height, rows::SESSION) as u16).min(area.height.saturating_sub(TOP_ROWS));
    let list = Rect::new(x, area.y + TOP_ROWS, width, list_h);
    if sessions.is_empty() && !resume.loading {
        let text = if resume.filter.buffer.is_empty() && !resume.all_projects {
            "No sessions in this folder yet. Tab shows every project."
        } else {
            "Nothing matches."
        };
        frame.render_widget(Paragraph::new(Span::styled(text, dim)), list);
    } else {
        // Окно сохранено с прошлой высоты; если курсор за ним (окно сжали) — к курсору.
        let selected = resume.cursor.selected;
        let mut window = resume.cursor.window(sessions.len(), usize::from(list_h));
        if !(window.start..window.end).contains(&selected) {
            window = ListWindow::anchored(selected, sessions.len(), usize::from(list_h));
        }
        let lines = list_lines(
            &sessions,
            window,
            resume.cursor.selected,
            usize::from(width),
            theme,
        );
        frame.render_widget(Paragraph::new(lines), list);
        render_scrollbar(frame, list, window, sessions.len(), theme);
    }

    let keys = format!(
        "{NAV_HINT_ARROWS} move \u{00B7} Enter open \u{00B7} Tab {} \u{00B7} Esc back",
        if resume.all_projects {
            "this folder"
        } else {
            "all projects"
        }
    );
    frame.render_widget(
        Paragraph::new(Span::styled(keys, dim)),
        Rect::new(x, area.bottom().saturating_sub(1), width, 1),
    );
    // Курсор — там, где правит набор: перед знаком под `filter.cursor`.
    let before: String = resume
        .filter
        .buffer
        .chars()
        .take(resume.filter.cursor)
        .collect();
    let typed = unicode_width::UnicodeWidthStr::width(before.as_str());
    let prompt_w = unicode_width::UnicodeWidthStr::width(prompt.as_str());
    let cursor_x = (x + (prompt_w + typed) as u16).min(area.right().saturating_sub(1));
    Some((cursor_x, filter_y))
}

/// Имя папки для подписи: последний кусок пути.
fn folder_name(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_string()
}

fn list_lines(
    sessions: &[&crate::session::SessionSummary],
    window: ListWindow,
    selected: usize,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let wide = width >= WIDE_COLS;
    let layout = RowLayout {
        number_cols: sessions.len().to_string().len(),
        model: wide,
        messages: wide,
        trail: SCROLLBAR_TRAIL,
    };
    let today = chrono::Local::now().date_naive();
    (window.start..window.end)
        .map(|index| {
            session_line(
                index + 1,
                sessions[index],
                index == selected,
                width,
                layout,
                today,
                theme,
            )
        })
        .collect()
}

/// Полоса прокрутки в последней колонке списка: только когда всё не влезает.
fn render_scrollbar(frame: &mut Frame, list: Rect, window: ListWindow, len: usize, theme: &Theme) {
    let height = usize::from(list.height);
    if len <= height || height == 0 || list.width == 0 {
        return;
    }
    let thumb_len = (height * height / len).max(1);
    let thumb_start = (window.start * height / len).min(height - thumb_len);
    let lines: Vec<Line> = (0..height)
        .map(|row| {
            if (thumb_start..thumb_start + thumb_len).contains(&row) {
                Line::from(Span::styled(
                    glyphs::get().gauge_full,
                    Style::default().fg(theme.text_dim),
                ))
            } else {
                Line::from(Span::styled("\u{2502}", Style::default().fg(theme.border)))
            }
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(list.right() - 1, list.y, 1, list.height),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folder_name_is_the_last_piece_of_the_path() {
        assert_eq!(folder_name("D:\\Projects\\demo-api\\"), "demo-api");
        assert_eq!(folder_name("/home/me/work"), "work");
    }

    #[test]
    fn the_page_is_what_fits_between_the_fixed_rows() {
        // Клавиши листают на `list::page_rows`, экран рисует столько же.
        assert_eq!(page_rows(24, rows::SESSION), usize::from(24 - TOP_ROWS - 2));
    }

    /// Длинный список листается под неподвижными шапкой и фильтром, полоса справа видна.
    #[test]
    fn a_long_list_scrolls_under_fixed_rows_with_a_scrollbar() {
        use crate::app::conversation::Conversation;
        use crate::app::list::ListNav;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (cols, rows) = (80u16, 16u16);
        let mut state = AppState::new(Conversation::fresh_main(None));
        state.workspace_path = "D:\\demo".to_string();
        state.resume.open();
        state.resume.apply(
            (0..30)
                .map(|n| crate::session::SessionSummary {
                    id: format!("s{n}"),
                    created_at: String::new(),
                    updated_at: String::new(),
                    workspace_root: "D:\\demo".to_string(),
                    message_count: 1,
                    title: format!("session {n:02}"),
                    model_type: String::new(),
                    tag: None,
                    broken: false,
                })
                .collect(),
        );
        for _ in 0..20 {
            state
                .resume
                .navigate(ListNav::Down, 30, page_rows(rows, rows::SESSION));
        }
        let theme = Theme::default_dark();
        let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        terminal
            .draw(|frame| {
                render_resume_view(frame, frame.area(), &state, &theme);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: Vec<String> = (0..rows)
            .map(|y| (0..cols).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        assert!(text[2].contains("21 of 30"), "{text:#?}");
        assert!(text[4].contains("filter"), "{text:#?}");
        assert!(text.iter().any(|l| l.contains("session 20")), "{text:#?}");
        assert!(!text.iter().any(|l| l.contains("session 00")), "{text:#?}");
        let bar_col = cols - MARGIN - 1;
        let bar: String = (TOP_ROWS..rows - 2)
            .map(|y| buffer[(bar_col, y)].symbol().to_string())
            .collect();
        assert!(bar.contains(glyphs::get().gauge_full), "{bar}");
    }
}
