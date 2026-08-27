//! Словарь навигации по спискам и математика окна прокрутки — на все панели.
//! Действия (`Enter`, `Space`, буквы) сюда не входят: они у каждой панели свои.

use crossterm::event::KeyCode;

/// Высота одной строки списка по панелям. Клавиши и отрисовка читают её
/// отсюда вместе — иначе шаг страницы разойдётся с тем, что видно на экране.
pub mod rows {
    pub const SERVER: u16 = 1;
    pub const PROVIDER: u16 = 1;
    /// Тема занимает две строки: название и описание.
    pub const THEME: u16 = 2;
    /// Результат поиска — три: заголовок, фрагмент, отбивка.
    pub const MATCH: u16 = 3;
}

/// Служебные строки полноэкранной панели: рамки шапки и футера.
const PANEL_CHROME_ROWS: u16 = 8;

/// Сколько строк списка помещается на экран высотой `terminal_rows`.
/// `rows_per_item` — высота одной строки списка (у тем 2, у поиска 3).
pub fn page_rows(terminal_rows: u16, rows_per_item: u16) -> usize {
    let body = terminal_rows.saturating_sub(PANEL_CHROME_ROWS);
    (body / rows_per_item.max(1)).max(1) as usize
}

/// Подсказка для футера панели с буквенными алиасами.
/// Живёт рядом со словарём, иначе снова разъедется с ним.
pub const NAV_HINT: &str = "j/k \u{2191}\u{2193} g/G PgUp/Dn";

/// То же для списка, который фильтруется набором текста: букв там нет.
pub const NAV_HINT_ARROWS: &str = "\u{2191}\u{2193} Home/End PgUp/Dn";

/// Навигационное намерение — единственное, что у всех списков одинаково.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListNav {
    Up,
    Down,
    PageUp,
    PageDown,
    First,
    Last,
}

impl ListNav {
    /// Только стрелки и Home/End/PageUp/PageDown — для списка,
    /// который принимает набор текста: там буква это символ запроса.
    pub fn from_code(code: KeyCode) -> Option<Self> {
        match code {
            KeyCode::Up => Some(Self::Up),
            KeyCode::Down => Some(Self::Down),
            KeyCode::PageUp => Some(Self::PageUp),
            KeyCode::PageDown => Some(Self::PageDown),
            KeyCode::Home => Some(Self::First),
            KeyCode::End => Some(Self::Last),
            _ => None,
        }
    }

    /// То же плюс vim-алиасы `k/j/g/G` — для панелей, где буквы свободны.
    pub fn from_code_vim(code: KeyCode) -> Option<Self> {
        match code {
            KeyCode::Char('k') => Some(Self::Up),
            KeyCode::Char('j') => Some(Self::Down),
            KeyCode::Char('g') => Some(Self::First),
            KeyCode::Char('G') => Some(Self::Last),
            other => Self::from_code(other),
        }
    }

    /// Новая позиция курсора — для панелей, что окно нигде не хранят.
    pub fn move_within(self, selected: usize, len: usize, page: usize) -> usize {
        let max = len.saturating_sub(1);
        let page = page.max(1);
        match self {
            Self::Up => selected.saturating_sub(1),
            Self::Down => (selected + 1).min(max),
            Self::PageUp => selected.saturating_sub(page),
            Self::PageDown => (selected + page).min(max),
            Self::First => 0,
            Self::Last => max,
        }
    }
}

/// Курсор списка вместе с окном прокрутки. Длину списка не хранит —
/// строки почти везде пересобираются заново, поэтому она приходит параметром.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListCursor {
    pub selected: usize,
    /// Начало окна. Снаружи читается только через `window()`.
    scroll: usize,
}

impl ListCursor {
    /// Применить перемещение. `visible` — высота окна в строках списка.
    pub fn apply(&mut self, nav: ListNav, len: usize, visible: usize) {
        self.selected = nav.move_within(self.selected, len, visible);
        self.follow(len, visible);
    }

    /// Подтянуть окно к курсору и загнать оба в границы — после того как
    /// список сменился под ними (фильтр, удаление, перезагрузка).
    pub fn clamp(&mut self, len: usize, visible: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
        self.follow(len, visible);
    }

    /// Поставить курсор на `index` и подтянуть окно — для списка со своей
    /// политикой перемещения (автодополнение ходит по кругу).
    pub fn move_to(&mut self, index: usize, len: usize, visible: usize) {
        self.selected = index.min(len.saturating_sub(1));
        self.follow(len, visible);
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Видимый срез при сохранённом окне: оно стоит на месте, пока курсор
    /// внутри, — список не дёргается на каждое нажатие.
    pub fn window(&self, len: usize, visible: usize) -> ListWindow {
        ListWindow::new(self.scroll, len, visible)
    }

    /// Держать `scroll` таким, чтобы `selected` оставался в окне.
    fn follow(&mut self, len: usize, visible: usize) {
        let page = visible.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + page {
            self.scroll = self.selected + 1 - page;
        }
        // Список мог укоротиться — не оставлять окно за его концом.
        self.scroll = self.scroll.min(len.saturating_sub(1));
    }
}

/// Срез списка, попадающий на экран, и признаки обрезки с обеих сторон.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListWindow {
    pub start: usize,
    pub end: usize,
    pub more_above: bool,
    pub more_below: bool,
}

impl ListWindow {
    fn new(start: usize, len: usize, visible: usize) -> Self {
        // Не оставлять пустоту под концом списка: сохранённое смещение могло
        // прийти от окна поменьше — терминал с тех пор растянули.
        // Окно нулевой высоты рисует пустоту, а не одну строку мимо области.
        let start = start.min(len.saturating_sub(visible.max(1)));
        let end = (start + visible).min(len);
        Self {
            start,
            end,
            more_above: start > 0,
            more_below: end < len,
        }
    }

    /// Окно, выведенное из одного курсора: высота полноэкранной панели
    /// известна только на отрисовке, запоминать его между кадрами нечему.
    pub fn anchored(selected: usize, len: usize, visible: usize) -> Self {
        Self::new(
            selected.saturating_sub(visible.saturating_sub(1)),
            len,
            visible,
        )
    }

    /// Индексы среза — чтобы вызывающий не собирал диапазон руками.
    pub fn range(&self) -> std::ops::Range<usize> {
        self.start..self.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_map_without_letters() {
        assert_eq!(ListNav::from_code(KeyCode::Up), Some(ListNav::Up));
        assert_eq!(ListNav::from_code(KeyCode::Home), Some(ListNav::First));
        assert_eq!(
            ListNav::from_code(KeyCode::PageDown),
            Some(ListNav::PageDown)
        );
        // Буквы принадлежат строке поиска, а не навигации.
        assert_eq!(ListNav::from_code(KeyCode::Char('j')), None);
        assert_eq!(ListNav::from_code(KeyCode::Char('g')), None);
    }

    #[test]
    fn vim_aliases_extend_the_arrow_set() {
        assert_eq!(
            ListNav::from_code_vim(KeyCode::Char('j')),
            Some(ListNav::Down)
        );
        assert_eq!(
            ListNav::from_code_vim(KeyCode::Char('k')),
            Some(ListNav::Up)
        );
        assert_eq!(
            ListNav::from_code_vim(KeyCode::Char('g')),
            Some(ListNav::First)
        );
        assert_eq!(
            ListNav::from_code_vim(KeyCode::Char('G')),
            Some(ListNav::Last)
        );
        assert_eq!(ListNav::from_code_vim(KeyCode::Up), Some(ListNav::Up));
        assert_eq!(ListNav::from_code_vim(KeyCode::Char('x')), None);
    }

    #[test]
    fn move_to_places_the_cursor_and_pulls_the_window() {
        let mut cursor = ListCursor::default();
        cursor.move_to(17, 20, 5);
        assert_eq!(cursor.selected, 17);
        assert_eq!(cursor.window(20, 5).range(), 13..18);
        // За концом списка курсор не встаёт.
        cursor.move_to(99, 3, 5);
        assert_eq!(cursor.selected, 2);
    }

    #[test]
    fn cursor_stops_at_both_ends() {
        let mut cursor = ListCursor::default();
        cursor.apply(ListNav::Up, 5, 3);
        assert_eq!(cursor.selected, 0);
        for _ in 0..10 {
            cursor.apply(ListNav::Down, 5, 3);
        }
        assert_eq!(cursor.selected, 4);
    }

    #[test]
    fn an_empty_list_keeps_the_cursor_at_zero() {
        let mut cursor = ListCursor::default();
        for nav in [ListNav::Down, ListNav::Last, ListNav::PageDown] {
            cursor.apply(nav, 0, 5);
            assert_eq!(cursor, ListCursor::default());
        }
    }

    #[test]
    fn paging_moves_by_one_window() {
        let mut cursor = ListCursor::default();
        cursor.apply(ListNav::PageDown, 100, 10);
        assert_eq!(cursor.selected, 10);
        cursor.apply(ListNav::PageUp, 100, 10);
        assert_eq!(cursor.selected, 0);
    }

    #[test]
    fn window_stays_put_while_the_cursor_is_inside_it() {
        let mut cursor = ListCursor::default();
        for _ in 0..4 {
            cursor.apply(ListNav::Down, 20, 5);
        }
        // Курсор дошёл до последней видимой строки, окно ещё не поехало.
        assert_eq!(cursor.selected, 4);
        assert_eq!(cursor.scroll, 0);
        cursor.apply(ListNav::Down, 20, 5);
        assert_eq!((cursor.selected, cursor.scroll), (5, 1));
    }

    #[test]
    fn window_follows_the_cursor_back_up() {
        let mut cursor = ListCursor {
            selected: 10,
            scroll: 6,
        };
        cursor.apply(ListNav::First, 20, 5);
        assert_eq!((cursor.selected, cursor.scroll), (0, 0));
    }

    #[test]
    fn last_scrolls_the_window_to_the_end() {
        let mut cursor = ListCursor::default();
        cursor.apply(ListNav::Last, 20, 5);
        assert_eq!(cursor.selected, 19);
        let window = cursor.window(20, 5);
        assert_eq!((window.start, window.end), (15, 20));
        assert!(window.more_above && !window.more_below);
    }

    #[test]
    fn clamp_pulls_a_stale_cursor_back_into_a_shrunken_list() {
        let mut cursor = ListCursor {
            selected: 18,
            scroll: 14,
        };
        cursor.clamp(3, 5);
        assert_eq!(cursor.selected, 2);
        let window = cursor.window(3, 5);
        // Три строки в окне на пять показываются целиком.
        assert_eq!((window.start, window.end), (0, 3));
        assert!(!window.more_above && !window.more_below);
    }

    #[test]
    fn a_grown_window_stops_showing_emptiness_under_the_list() {
        // Смещение доехало на узком окне, потом терминал растянули: раньше
        // окно на десять строк показывало три последних и пустоту под ними.
        let mut cursor = ListCursor::default();
        for _ in 0..8 {
            cursor.apply(ListNav::Down, 10, 2);
        }
        let window = cursor.window(10, 10);
        assert_eq!((window.start, window.end), (0, 10));
        assert!(!window.more_above && !window.more_below);
    }

    #[test]
    fn anchored_window_needs_no_stored_scroll() {
        let window = ListWindow::anchored(0, 20, 5);
        assert_eq!((window.start, window.end), (0, 5));
        assert!(!window.more_above && window.more_below);

        let window = ListWindow::anchored(7, 20, 5);
        assert_eq!((window.start, window.end), (3, 8));
        assert!(window.more_above && window.more_below);

        let window = ListWindow::anchored(19, 20, 5);
        assert_eq!((window.start, window.end), (15, 20));
        assert!(window.more_above && !window.more_below);
    }

    #[test]
    fn a_window_over_nothing_is_empty_and_reports_no_overflow() {
        let window = ListWindow::anchored(0, 0, 5);
        assert_eq!((window.start, window.end), (0, 0));
        assert!(!window.more_above && !window.more_below);
        assert_eq!(window.range().count(), 0);
    }

    #[test]
    fn a_zero_height_window_draws_nothing() {
        // Раньше окно округлялось до одной строки и рисовало её мимо области.
        let window = ListWindow::anchored(3, 10, 0);
        assert_eq!(window.range().count(), 0);
    }

    #[test]
    fn page_rows_follows_the_terminal_and_the_row_height() {
        assert_eq!(page_rows(40, 1), 32);
        assert_eq!(page_rows(40, 2), 16);
        assert_eq!(page_rows(40, 3), 10);
        // Крошечный терминал всё равно даёт шаг в одну строку, а не ноль.
        assert_eq!(page_rows(4, 1), 1);
        assert_eq!(page_rows(0, 3), 1);
    }

    #[test]
    fn a_list_shorter_than_the_window_is_shown_whole() {
        let window = ListWindow::anchored(1, 3, 10);
        assert_eq!((window.start, window.end), (0, 3));
        assert!(!window.more_above && !window.more_below);
    }
}
