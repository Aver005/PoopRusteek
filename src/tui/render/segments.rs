//! Подбор частей строки по ширине: что не помещается, сначала сокращается,
//! потом отбрасывается — начиная с наименее важного. Чистая логика без
//! рисования, на ней стоит нижняя строка и строка сводки в панели работы.

use unicode_width::UnicodeWidthStr;

/// Как красить часть строки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Plain,
    Dim,
    Accent,
    Warning,
    Error,
}

/// Одна часть строки. `variants` — от полного текста к самому короткому;
/// чем больше `priority`, тем раньше часть сокращается и отбрасывается.
/// С `priority == 0` часть не трогают никогда.
#[derive(Debug, Clone)]
pub(crate) struct Segment {
    pub priority: u8,
    pub variants: Vec<String>,
    pub tone: Tone,
}

impl Segment {
    pub(crate) fn new(priority: u8, tone: Tone, text: impl Into<String>) -> Self {
        Self {
            priority,
            variants: vec![text.into()],
            tone,
        }
    }

    /// Запасной, более короткий текст.
    pub(crate) fn or_short(mut self, text: impl Into<String>) -> Self {
        self.variants.push(text.into());
        self
    }
}

/// Часть, которая попала в строку.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fitted {
    pub text: String,
    pub tone: Tone,
}

struct Slot {
    right: bool,
    segment: Segment,
    variant: usize,
    alive: bool,
}

impl Slot {
    fn text(&self) -> &str {
        self.segment
            .variants
            .get(self.variant)
            .map_or("", String::as_str)
    }
}

/// Расставить части слева и справа в `width` клеток. Между группами нужна
/// хотя бы одна клетка. Порядок внутри групп сохраняется.
pub(crate) fn fit_segments(
    width: usize,
    left: Vec<Segment>,
    right: Vec<Segment>,
) -> (Vec<Fitted>, Vec<Fitted>) {
    let mut slots: Vec<Slot> = left
        .into_iter()
        .map(|segment| (false, segment))
        .chain(right.into_iter().map(|segment| (true, segment)))
        .filter(|(_, segment)| segment.variants.iter().any(|text| !text.is_empty()))
        .map(|(right, segment)| Slot {
            right,
            segment,
            variant: 0,
            alive: true,
        })
        .collect();

    let total = |slots: &[Slot]| -> usize {
        let used: usize = slots
            .iter()
            .filter(|slot| slot.alive)
            .map(|slot| UnicodeWidthStr::width(slot.text()))
            .sum();
        let both = slots.iter().any(|slot| slot.alive && !slot.right)
            && slots.iter().any(|slot| slot.alive && slot.right);
        used + usize::from(both)
    };

    while total(&slots) > width {
        // Самая «дешёвая» часть: наибольший priority, при равенстве — правее.
        let Some(index) = slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.alive && slot.segment.priority > 0)
            .max_by_key(|(index, slot)| (slot.segment.priority, *index))
            .map(|(index, _)| index)
        else {
            break;
        };
        let slot = &mut slots[index];
        if slot.variant + 1 < slot.segment.variants.len() {
            slot.variant += 1;
        } else {
            slot.alive = false;
        }
    }

    let collect = |right: bool| -> Vec<Fitted> {
        slots
            .iter()
            .filter(|slot| slot.alive && slot.right == right)
            .map(|slot| Fitted {
                text: slot.text().to_string(),
                tone: slot.segment.tone,
            })
            .collect()
    };
    (collect(false), collect(true))
}

/// Ширина набора частей в клетках.
pub(crate) fn fitted_width(parts: &[Fitted]) -> usize {
    parts
        .iter()
        .map(|part| UnicodeWidthStr::width(part.text.as_str()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(parts: &[Fitted]) -> Vec<&str> {
        parts.iter().map(|part| part.text.as_str()).collect()
    }

    fn seg(priority: u8, text: &str) -> Segment {
        Segment::new(priority, Tone::Plain, text)
    }

    #[test]
    fn everything_fits_when_there_is_room() {
        let (left, right) = fit_segments(
            40,
            vec![seg(1, "model"), seg(3, " · mcp")],
            vec![seg(2, "ctx")],
        );
        assert_eq!(texts(&left), ["model", " · mcp"]);
        assert_eq!(texts(&right), ["ctx"]);
    }

    #[test]
    fn the_least_important_part_goes_first() {
        let (left, right) = fit_segments(
            13,
            vec![seg(1, "model"), seg(3, " · mcp 2/3")],
            vec![seg(2, "ctx 38%")],
        );
        assert_eq!(texts(&left), ["model"]);
        assert_eq!(texts(&right), ["ctx 38%"]);
    }

    #[test]
    fn a_part_shrinks_to_a_shorter_variant_before_it_is_dropped() {
        let long = Segment::new(2, Tone::Plain, "ctx ███░░░░░ 38%").or_short("ctx 38%");
        let (_, right) = fit_segments(9, vec![], vec![long]);
        assert_eq!(texts(&right), ["ctx 38%"]);
    }

    #[test]
    fn a_priority_zero_part_is_never_dropped() {
        let (left, right) = fit_segments(4, vec![seg(0, "ERROR!!!")], vec![seg(5, "time")]);
        assert_eq!(texts(&left), ["ERROR!!!"]);
        assert!(right.is_empty());
    }

    #[test]
    fn the_two_groups_keep_a_gap_between_them() {
        let (left, right) = fit_segments(9, vec![seg(1, "abcd")], vec![seg(2, "efgh")]);
        // 4 + 1 + 4 = 9 помещается ровно.
        assert_eq!(texts(&left), ["abcd"]);
        assert_eq!(texts(&right), ["efgh"]);
        let (_, right) = fit_segments(8, vec![seg(1, "abcd")], vec![seg(2, "efgh")]);
        assert!(right.is_empty(), "без зазора правая группа не помещается");
    }

    #[test]
    fn width_is_measured_in_cells_not_chars() {
        let (_, right) = fit_segments(3, vec![], vec![seg(1, "你好")]);
        assert!(right.is_empty(), "две широкие буквы занимают четыре клетки");
    }

    #[test]
    fn empty_parts_are_skipped() {
        let (left, _) = fit_segments(10, vec![seg(1, ""), seg(1, "a")], vec![]);
        assert_eq!(texts(&left), ["a"]);
    }

    #[test]
    fn the_result_always_fits_or_holds_only_protected_parts() {
        for width in 0..40 {
            let (left, right) = fit_segments(
                width,
                vec![
                    seg(1, "DeepSeek · deepseek-chat"),
                    seg(3, " · mcp 2/3"),
                    seg(4, " · chats 2"),
                ],
                vec![seg(2, "ctx 38%"), seg(5, "14:02")],
            );
            let used = fitted_width(&left) + fitted_width(&right);
            assert!(used <= width, "width {width}, used {used}");
        }
    }
}
