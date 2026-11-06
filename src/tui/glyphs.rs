//! Знаки интерфейса в двух наборах: Unicode и «безопасный» ASCII.
//! Консоль Windows со шрифтом по умолчанию рисует `✓ ✗ ⚠ ━` квадратами, так
//! что каждый нестандартный знак берётся отсюда, а не пишется в виджете.

use crate::config::GlyphMode;
use std::cell::Cell;

/// Один набор знаков.
#[derive(Debug, PartialEq, Eq)]
pub struct Glyphs {
    pub ascii: bool,
    pub check: &'static str,
    pub cross: &'static str,
    pub warn: &'static str,
    pub info: &'static str,
    pub tool: &'static str,
    pub clip: &'static str,
    pub alarm: &'static str,
    /// Толстая линия заголовков разделов.
    pub heavy_rule: &'static str,
    /// Полоса слева от ответа ассистента.
    pub bar: &'static str,
    /// Полоса слева от цитаты.
    pub quote_bar: &'static str,
    pub prompt: &'static str,
    pub pointer: &'static str,
    pub checked: &'static str,
    pub unchecked: &'static str,
    pub radio_on: &'static str,
    pub radio_off: &'static str,
    pub spinner: &'static [&'static str],
}

pub static UNICODE: Glyphs = Glyphs {
    ascii: false,
    check: "\u{2713}",
    cross: "\u{2717}",
    warn: "\u{26A0}",
    info: "\u{2139}",
    tool: "\u{2692}",
    clip: "\u{1F4CE}",
    alarm: "\u{23F0}",
    heavy_rule: "\u{2501}",
    bar: "\u{258D}",
    quote_bar: "\u{258F}",
    prompt: "\u{276F}",
    pointer: "\u{25B6}",
    checked: "\u{2611}",
    unchecked: "\u{2610}",
    radio_on: "\u{25CF}",
    radio_off: "\u{25CB}",
    spinner: &[
        "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283C}", "\u{2834}", "\u{2826}",
        "\u{2827}", "\u{2807}", "\u{280F}",
    ],
};

/// Только ASCII и знаки, которые есть в CP437 (рамки `─ │ ├ └`, `·`, стрелки):
/// их рисует любой шрифт консоли.
pub static ASCII: Glyphs = Glyphs {
    ascii: true,
    check: "+",
    cross: "x",
    warn: "!",
    info: "i",
    tool: "*",
    clip: "&",
    alarm: "~",
    heavy_rule: "\u{2500}",
    bar: "|",
    quote_bar: "|",
    prompt: ">",
    pointer: ">",
    checked: "[x]",
    unchecked: "[ ]",
    radio_on: "(*)",
    radio_off: "( )",
    spinner: &["|", "/", "-", "\\"],
};

thread_local! {
    /// Набор текущего кадра. Кадр рисуется в одном потоке; `draw_frame`
    /// выставляет его из конфига перед рисованием.
    static ACTIVE: Cell<&'static Glyphs> = const { Cell::new(&UNICODE) };
}

/// Нужен ли ASCII-набор при `auto`: Windows и терминал, о котором не известно,
/// что он рисует Unicode (Windows Terminal, VS Code, ConEmu).
fn auto_prefers_ascii(is_windows: bool, modern_terminal: bool) -> bool {
    is_windows && !modern_terminal
}

fn modern_terminal() -> bool {
    ["WT_SESSION", "TERM_PROGRAM", "ConEmuANSI"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
}

/// Набор для режима из конфига.
pub fn for_mode(mode: GlyphMode) -> &'static Glyphs {
    match mode {
        GlyphMode::Unicode => &UNICODE,
        GlyphMode::Ascii => &ASCII,
        GlyphMode::Auto => {
            if auto_prefers_ascii(cfg!(windows), modern_terminal()) {
                &ASCII
            } else {
                &UNICODE
            }
        }
    }
}

/// Выбрать набор кадра.
pub fn select(mode: GlyphMode) {
    ACTIVE.with(|active| active.set(for_mode(mode)));
}

/// Набор текущего кадра.
pub fn get() -> &'static Glyphs {
    ACTIVE.with(Cell::get)
}

impl Glyphs {
    /// Кадр спиннера для такта анимации.
    pub fn spin(&self, tick: u64) -> &'static str {
        self.spinner[(tick as usize) % self.spinner.len()]
    }

    /// Знак «успех» или «ошибка» для результата инструмента.
    pub fn outcome(&self, failed: bool) -> &'static str {
        if failed { self.cross } else { self.check }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Знаки вне ASCII, которые безопасны в консоли со шрифтом по умолчанию.
    const SAFE_NON_ASCII: &[char] = &['\u{2500}', '\u{2502}', '\u{251C}', '\u{2514}', '\u{00B7}'];

    fn all_glyphs(set: &Glyphs) -> Vec<&'static str> {
        let mut all = vec![
            set.check,
            set.cross,
            set.warn,
            set.info,
            set.tool,
            set.clip,
            set.alarm,
            set.heavy_rule,
            set.bar,
            set.quote_bar,
            set.prompt,
            set.pointer,
            set.checked,
            set.unchecked,
            set.radio_on,
            set.radio_off,
        ];
        all.extend(set.spinner);
        all
    }

    #[test]
    fn the_ascii_set_has_only_safe_characters() {
        for glyph in all_glyphs(&ASCII) {
            for ch in glyph.chars() {
                assert!(
                    ch.is_ascii() || SAFE_NON_ASCII.contains(&ch),
                    "unsafe glyph {ch:?} in the ascii set"
                );
            }
        }
    }

    #[test]
    fn both_sets_fill_every_slot() {
        for set in [&UNICODE, &ASCII] {
            for glyph in all_glyphs(set) {
                assert!(!glyph.is_empty());
            }
            assert!(!set.spinner.is_empty());
        }
    }

    #[test]
    fn auto_picks_ascii_only_for_an_old_windows_console() {
        assert!(auto_prefers_ascii(true, false));
        assert!(!auto_prefers_ascii(true, true));
        assert!(!auto_prefers_ascii(false, false));
    }

    #[test]
    fn explicit_modes_ignore_the_terminal() {
        assert!(!for_mode(GlyphMode::Unicode).ascii);
        assert!(for_mode(GlyphMode::Ascii).ascii);
    }

    #[test]
    fn the_spinner_cycles_without_overflow() {
        let set = &UNICODE;
        assert_eq!(set.spin(0), set.spin(set.spinner.len() as u64));
        assert_eq!(ASCII.spin(u64::MAX), ASCII.spinner[(u64::MAX as usize) % 4]);
    }

    #[test]
    fn select_changes_what_get_returns() {
        select(GlyphMode::Ascii);
        assert!(get().ascii);
        select(GlyphMode::Unicode);
        assert!(!get().ascii);
    }
}
