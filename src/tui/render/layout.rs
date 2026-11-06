//! Единый скелет экрана для главной и чата. Все высоты считаются здесь, в
//! одном месте, а поле ввода всегда прижато к низу: на любом размере окна и на
//! любом экране его последняя строка лежит прямо над строкой подвала.
//!
//! ```text
//! transcript            (всё, что осталось)   sidebar — справа, если есть место
//! attach                (0 или 1 строка)
//! dock                  (0..4 строки, только пока идёт работа)
//! rule                  (1 строка: подпись и подсказки клавиш)
//! input                 (1..5 строк, растёт вверх)
//! footer                (1 строка)
//! ```

use super::dock::max_rows as max_dock_rows;
use ratatui::layout::Rect;

/// Сколько строк занимает поле ввода, пока не начнёт прокручиваться.
pub(crate) const MAX_INPUT_ROWS: u16 = 5;
/// Ширина боковой панели.
pub(crate) const SIDEBAR_WIDTH: u16 = 34;
/// С этой ширины окна боковая панель включена сама.
pub(crate) const SIDEBAR_AUTO_COLS: u16 = 140;
/// Пока чату остаётся меньше колонок, панель не рисуется.
const MIN_TRANSCRIPT_COLS_WITH_SIDEBAR: u16 = 60;
/// С такой ширины окна панель вообще помещается (вместе с разделителем).
pub(crate) const SIDEBAR_MIN_COLS: u16 = SIDEBAR_WIDTH + 1 + MIN_TRANSCRIPT_COLS_WITH_SIDEBAR;
/// Столько строк под переписку оставляем при любых аппетитах остальных.
const MIN_TRANSCRIPT_ROWS: u16 = 3;

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
    pub transcript: Rect,
    pub sidebar: Option<Rect>,
    pub attach: Rect,
    pub dock: Rect,
    pub rule: Rect,
    pub input: Rect,
    pub footer: Rect,
}

impl Regions {
    /// Раскладка для области `area`. Нижний край не зависит от `needs`.
    pub(crate) fn compute(area: Rect, needs: &Needs) -> Self {
        let footer_h = u16::from(area.height > 0);
        let rule_h = u16::from(area.height > footer_h);
        // Одна строка ввода есть всегда, пока хватает высоты.
        let input_min = u16::from(area.height > footer_h + rule_h);
        let fixed = footer_h + rule_h + input_min;
        let spare = area.height.saturating_sub(fixed);
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
        let bottom = area.bottom();
        let footer = Rect::new(area.x, bottom - footer_h, area.width, footer_h);
        let input = Rect::new(area.x, footer.y - input_h, area.width, input_h);
        let rule = Rect::new(area.x, input.y - rule_h, area.width, rule_h);
        let dock = Rect::new(area.x, rule.y - dock_h, area.width, dock_h);
        let attach = Rect::new(area.x, dock.y - attach_h, area.width, attach_h);
        let transcript_h = attach.y - area.y;
        let transcript_full = Rect::new(area.x, area.y, area.width, transcript_h);

        let sidebar_w = SIDEBAR_WIDTH + 1;
        let sidebar =
            (needs.sidebar && area.width >= SIDEBAR_MIN_COLS && transcript_h > 0).then(|| {
                Rect::new(
                    transcript_full.right() - SIDEBAR_WIDTH,
                    transcript_full.y,
                    SIDEBAR_WIDTH,
                    transcript_h,
                )
            });
        let transcript = match sidebar {
            Some(_) => Rect::new(
                transcript_full.x,
                transcript_full.y,
                transcript_full.width - sidebar_w,
                transcript_h,
            ),
            None => transcript_full,
        };
        Self {
            transcript,
            sidebar,
            attach,
            dock,
            rule,
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
    fn the_input_sits_right_above_the_footer_for_every_need() {
        for (w, h) in SIZES {
            let area = Rect::new(0, 0, w, h);
            for dock in [0, 1, 4] {
                for input in [1, 3, 9] {
                    for attach in [false, true] {
                        for sidebar in [false, true] {
                            let r = Regions::compute(area, &needs(dock, input, attach, sidebar));
                            assert_eq!(r.footer.bottom(), h, "{w}x{h}");
                            assert_eq!(r.input.bottom(), r.footer.y, "{w}x{h}");
                            assert_eq!(r.rule.bottom(), r.input.y, "{w}x{h}");
                            assert_eq!(r.dock.bottom(), r.rule.y, "{w}x{h}");
                            assert_eq!(r.attach.bottom(), r.dock.y, "{w}x{h}");
                            assert_eq!(r.transcript.bottom(), r.attach.y, "{w}x{h}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn an_idle_screen_has_the_same_composer_everywhere() {
        // Один ряд ввода и подвал: последняя строка ввода — вторая снизу.
        for (w, h) in SIZES {
            let r = Regions::compute(Rect::new(0, 0, w, h), &needs(0, 1, false, false));
            assert_eq!(r.input.height, 1);
            assert_eq!(r.input.y, h - 2, "{w}x{h}");
            assert_eq!(r.rule.y, h - 3, "{w}x{h}");
        }
    }

    #[test]
    fn the_input_grows_upward_and_stops_at_the_limit() {
        let area = Rect::new(0, 0, 100, 40);
        let r = Regions::compute(area, &needs(0, 99, false, false));
        assert_eq!(r.input.height, MAX_INPUT_ROWS);
        assert_eq!(r.input.bottom(), 39);
    }

    #[test]
    fn regions_never_overlap_or_leave_the_screen() {
        for (w, h) in SIZES {
            let area = Rect::new(0, 0, w, h);
            let r = Regions::compute(area, &needs(4, 5, true, true));
            let total = r.transcript.height
                + r.attach.height
                + r.dock.height
                + r.rule.height
                + r.input.height
                + r.footer.height;
            assert_eq!(total, h, "{w}x{h}");
            assert!(
                r.transcript.height >= MIN_TRANSCRIPT_ROWS || h < 10,
                "{w}x{h}"
            );
            if let Some(side) = r.sidebar {
                assert!(r.transcript.right() < side.x, "{w}x{h}");
                assert!(side.right() <= w, "{w}x{h}");
            }
        }
    }

    #[test]
    fn the_sidebar_needs_room_for_the_chat() {
        let narrow = Regions::compute(Rect::new(0, 0, 80, 40), &needs(0, 1, false, true));
        assert!(narrow.sidebar.is_none());
        let wide = Regions::compute(Rect::new(0, 0, 160, 40), &needs(0, 1, false, true));
        assert_eq!(wide.sidebar.map(|rect| rect.width), Some(SIDEBAR_WIDTH));
    }

    #[test]
    fn tiny_heights_do_not_underflow() {
        for h in 0..6 {
            let _ = Regions::compute(Rect::new(0, 0, 40, h), &needs(4, 5, true, true));
        }
    }
}
