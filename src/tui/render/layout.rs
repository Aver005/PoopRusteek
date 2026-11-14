//! Единый скелет экрана чата. Все высоты и ширины считаются здесь, в одном месте:
//! шапка сверху, подвал снизу, между ними рабочая колонка с полями и, если есть
//! место, правая панель на всю высоту.
//!
//! ```text
//! header                         (1 строка: продукт, версия, название чата)
//!    transcript          │ sidebar   (колонка с полями по бокам; панель — справа)
//!    attach   (0..1)     │
//!    dock     (0..4)     │
//!    ╭ input ─────────╮  │           (рамка + 1..5 строк, растёт вверх)
//!    ╰────────────────╯  │
//! footer                         (1 строка: модель, контекст … провайдер)
//! ```

use super::dock::max_rows as max_dock_rows;
use ratatui::layout::Rect;

/// Сколько строк занимает поле ввода, пока не начнёт прокручиваться.
pub(crate) const MAX_INPUT_ROWS: u16 = 5;
/// Ширина правой панели: всегда одна и та же.
pub(crate) const SIDEBAR_WIDTH: u16 = 30;
/// С этой ширины окна панель включена сама.
pub(crate) const SIDEBAR_AUTO_COLS: u16 = 120;
/// Пока чату остаётся меньше колонок, панель не рисуется.
const MIN_COLUMN_COLS_WITH_SIDEBAR: u16 = 60;
/// С такой ширины окна панель вообще помещается (вместе с разделителем).
pub(crate) const SIDEBAR_MIN_COLS: u16 = SIDEBAR_WIDTH + 1 + MIN_COLUMN_COLS_WITH_SIDEBAR;
/// Поля рабочей колонки слева и справа; в узком окне — по одному.
pub(crate) const SIDE_MARGIN: u16 = 3;
const NARROW_COLUMN_COLS: u16 = 50;
/// Столько строк под переписку оставляем при любых аппетитах остальных.
const MIN_TRANSCRIPT_ROWS: u16 = 3;
/// Верхняя и нижняя граница рамки поля ввода.
const BOX_BORDERS: u16 = 2;

/// Что экран хочет занять.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Needs {
    pub dock_rows: u16,
    pub input_rows: u16,
    pub attachments: bool,
    pub sidebar: bool,
}

/// Куда что рисовать.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Regions {
    pub header: Rect,
    pub transcript: Rect,
    pub sidebar: Option<Rect>,
    pub attach: Rect,
    pub dock: Rect,
    /// Рамка поля ввода целиком; без рамки (совсем низкое окно) равна `input`.
    pub input_box: Rect,
    pub input: Rect,
    pub footer: Rect,
}

/// Поле колонки шириной `width`: в узком окне поля сужаются.
fn margin_for(width: u16) -> u16 {
    if width >= NARROW_COLUMN_COLS {
        SIDE_MARGIN
    } else {
        1.min(width / 2)
    }
}

impl Regions {
    /// Раскладка для области `area`. Подвал всегда последняя строка, шапка — первая.
    pub(crate) fn compute(area: Rect, needs: &Needs) -> Self {
        let header_h = u16::from(area.height > 0);
        let footer_h = u16::from(area.height > header_h);
        let header = Rect::new(area.x, area.y, area.width, header_h);
        let footer = Rect::new(area.x, area.bottom() - footer_h, area.width, footer_h);
        let body = Rect::new(
            area.x,
            header.bottom(),
            area.width,
            footer.y - header.bottom(),
        );

        let sidebar =
            (needs.sidebar && area.width >= SIDEBAR_MIN_COLS && body.height > 0).then(|| {
                Rect::new(
                    body.right() - SIDEBAR_WIDTH,
                    body.y,
                    SIDEBAR_WIDTH,
                    body.height,
                )
            });
        let column_full = match sidebar {
            Some(side) => Rect::new(body.x, body.y, side.x - 1 - body.x, body.height),
            None => body,
        };
        let margin = margin_for(column_full.width);
        let column = Rect::new(
            column_full.x + margin,
            column_full.y,
            column_full.width - 2 * margin,
            column_full.height,
        );

        // Рамка нужна, только пока под неё и под переписку хватает высоты.
        let borders = if column.height > BOX_BORDERS + MIN_TRANSCRIPT_ROWS {
            BOX_BORDERS
        } else {
            0
        };
        let input_min = u16::from(column.height > borders);
        let spare = column.height - borders - input_min;
        // Переписке отдаём минимум, остальное делим: ввод, вложения, панель работы.
        let free = spare.saturating_sub(MIN_TRANSCRIPT_ROWS);
        let extra_input = needs
            .input_rows
            .clamp(1, MAX_INPUT_ROWS)
            .saturating_sub(1)
            .min(free);
        let free = free - extra_input;
        let attach_h = u16::from(needs.attachments).min(free);
        let free = free - attach_h;
        let dock_h = needs.dock_rows.min(max_dock_rows(area.height)).min(free);

        let input_h = input_min + extra_input;
        let box_h = input_h + if input_h > 0 { borders } else { 0 };
        let input_box = Rect::new(column.x, column.bottom() - box_h, column.width, box_h);
        let input = if borders > 0 && input_h > 0 {
            Rect::new(
                input_box.x + 1,
                input_box.y + 1,
                input_box.width.saturating_sub(2),
                input_h,
            )
        } else {
            input_box
        };
        let dock = Rect::new(column.x, input_box.y - dock_h, column.width, dock_h);
        let attach = Rect::new(column.x, dock.y - attach_h, column.width, attach_h);
        let transcript = Rect::new(column.x, column.y, column.width, attach.y - column.y);
        Self {
            header,
            transcript,
            sidebar,
            attach,
            dock,
            input_box,
            input,
            footer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn needs(dock_rows: u16, input_rows: u16, attachments: bool, sidebar: bool) -> Needs {
        Needs {
            dock_rows,
            input_rows,
            attachments,
            sidebar,
        }
    }

    const SIZES: [(u16, u16); 7] = [
        (40, 10),
        (60, 20),
        (80, 24),
        (120, 40),
        (140, 40),
        (160, 50),
        (200, 60),
    ];

    #[test]
    fn the_column_stacks_from_the_header_down_to_the_footer_for_every_need() {
        for (w, h) in SIZES {
            let area = Rect::new(0, 0, w, h);
            for dock in [0, 1, 4] {
                for input in [1, 3, 9] {
                    for attach in [false, true] {
                        for sidebar in [false, true] {
                            let r = Regions::compute(area, &needs(dock, input, attach, sidebar));
                            assert_eq!(r.header.y, 0, "{w}x{h}");
                            assert_eq!(r.footer.bottom(), h, "{w}x{h}");
                            assert_eq!(r.transcript.y, r.header.bottom(), "{w}x{h}");
                            assert_eq!(r.input_box.bottom(), r.footer.y, "{w}x{h}");
                            assert_eq!(r.dock.bottom(), r.input_box.y, "{w}x{h}");
                            assert_eq!(r.attach.bottom(), r.dock.y, "{w}x{h}");
                            assert_eq!(r.transcript.bottom(), r.attach.y, "{w}x{h}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn an_idle_screen_has_one_input_row_inside_a_frame_above_the_footer() {
        for (w, h) in SIZES {
            let r = Regions::compute(Rect::new(0, 0, w, h), &needs(0, 1, false, false));
            assert_eq!(r.input.height, 1, "{w}x{h}");
            assert_eq!(r.input.y, h - 3, "{w}x{h}");
            assert_eq!(r.input_box.height, 3, "{w}x{h}");
        }
    }

    #[test]
    fn the_input_grows_upward_and_stops_at_the_limit() {
        let area = Rect::new(0, 0, 100, 40);
        let r = Regions::compute(area, &needs(0, 99, false, false));
        assert_eq!(r.input.height, MAX_INPUT_ROWS);
        assert_eq!(r.input.bottom(), 38);
    }

    #[test]
    fn the_column_keeps_its_side_margins() {
        let r = Regions::compute(Rect::new(0, 0, 100, 30), &needs(0, 1, false, false));
        assert_eq!(r.transcript.x, SIDE_MARGIN);
        assert_eq!(r.transcript.right(), 100 - SIDE_MARGIN);
        assert_eq!(r.input_box.x, r.transcript.x);
        assert_eq!(r.input_box.width, r.transcript.width);
    }

    #[test]
    fn regions_never_overlap_or_leave_the_screen() {
        for (w, h) in SIZES {
            let area = Rect::new(0, 0, w, h);
            let r = Regions::compute(area, &needs(4, 5, true, true));
            let total = r.header.height
                + r.transcript.height
                + r.attach.height
                + r.dock.height
                + r.input_box.height
                + r.footer.height;
            assert_eq!(total, h, "{w}x{h}");
            assert!(
                r.transcript.height >= MIN_TRANSCRIPT_ROWS || h < 10,
                "{w}x{h}"
            );
            if let Some(side) = r.sidebar {
                assert!(r.transcript.right() < side.x, "{w}x{h}");
                assert!(r.input_box.right() < side.x, "{w}x{h}");
                assert!(side.right() <= w, "{w}x{h}");
                assert_eq!(side.y, r.header.bottom(), "{w}x{h}");
                assert_eq!(side.bottom(), r.footer.y, "{w}x{h}");
            }
        }
    }

    #[test]
    fn the_sidebar_has_one_width_and_needs_room_for_the_chat() {
        let narrow = Regions::compute(Rect::new(0, 0, 80, 40), &needs(0, 1, false, true));
        assert!(narrow.sidebar.is_none());
        for w in [SIDEBAR_MIN_COLS, 120, 160, 200] {
            let wide = Regions::compute(Rect::new(0, 0, w, 40), &needs(0, 1, false, true));
            assert_eq!(wide.sidebar.map(|rect| rect.width), Some(SIDEBAR_WIDTH));
        }
    }

    #[test]
    fn tiny_heights_do_not_underflow() {
        for h in 0..8 {
            for w in [1, 2, 10, 40] {
                let _ = Regions::compute(Rect::new(0, 0, w, h), &needs(4, 5, true, true));
            }
        }
    }
}
