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
    /// Знак рядом с названием в заголовке окна и в предпросмотре темы.
    pub brand: &'static str,
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
    /// Полоса выбранной строки списка.
    pub sel_bar: &'static str,
    /// Аккаунт остывает.
    pub cooling: &'static str,
    /// Избранная модель.
    pub star: &'static str,
    /// Шкала лимита: свежее показание, оценка, пустое.
    pub gauge_full: &'static str,
    pub gauge_est: &'static str,
    pub gauge_empty: &'static str,
    /// Курсор набираемого ответа.
    pub caret: &'static str,
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
    brand: "\u{1F9FB}",
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
    sel_bar: "\u{258C}",
    cooling: "\u{25D4}",
    star: "\u{2605}",
    gauge_full: "\u{2588}",
    gauge_est: "\u{2592}",
    gauge_empty: "\u{2591}",
    caret: "\u{258D}",
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
    brand: "",
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
    sel_bar: ">",
    cooling: "~",
    star: "*",
    gauge_full: "#",
    gauge_est: "+",
    gauge_empty: "-",
    caret: "_",
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

/// Служебный текст с готовыми значками: в ASCII-режиме значки заменяются, иначе квадраты.
pub fn console_text(text: &str) -> std::borrow::Cow<'_, str> {
    const SIGNS: [(char, &str); 9] = [
        ('\u{26A0}', "!"),
        ('\u{2713}', "+"),
        ('\u{2717}', "x"),
        ('\u{2139}', "i"),
        ('\u{23F0}', "~"),
        ('\u{1F4CE}', "&"),
        ('\u{2692}', "*"),
        ('\u{1F512}', "#"),
        ('\u{1F4CB}', "*"),
    ];
    if !get().ascii
        || !text
            .chars()
            .any(|c| SIGNS.iter().any(|(sign, _)| *sign == c))
    {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match SIGNS.iter().find(|(sign, _)| *sign == c) {
            Some((_, plain)) => out.push_str(plain),
            None => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Маска скрытого ввода: точки по числу знаков, но не длиннее поля.
pub fn mask_text(len: usize) -> String {
    let dot = if get().ascii { "*" } else { "\u{2022}" };
    dot.repeat(len.min(48))
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
mod console_text_tests {
    use super::*;

    #[test]
    fn signs_in_a_note_are_replaced_only_in_ascii_mode() {
        select(GlyphMode::Ascii);
        assert_eq!(console_text("\u{26A0} models failed"), "! models failed");
        assert_eq!(console_text("Привет, мир"), "Привет, мир");
        select(GlyphMode::Unicode);
        assert_eq!(
            console_text("\u{26A0} models failed"),
            "\u{26A0} models failed"
        );
    }
}

/// Знаки вне ASCII, которые безопасны в консоли со шрифтом по умолчанию:
/// есть в CP437 или в обычных шрифтах (рамки, `·`, `…`, `—`, стрелки, блоки шкалы).
#[cfg(test)]
pub(crate) const CONSOLE_SAFE: &[char] = &[
    '\u{2500}', '\u{2502}', '\u{250C}', '\u{2510}', '\u{2514}', '\u{2518}', '\u{251C}', '\u{2524}',
    '\u{252C}', '\u{2534}', '\u{253C}', '\u{00B7}', '\u{2022}', '\u{2026}', '\u{2014}', '\u{2191}',
    '\u{2193}', '\u{2192}', '\u{2588}', '\u{2591}',
];

/// Символ можно показывать в ASCII-режиме: ASCII, кириллица (текст самих
/// фикстур) или знак из [`CONSOLE_SAFE`].
#[cfg(test)]
pub(crate) fn is_console_safe(ch: char) -> bool {
    ch.is_ascii()
        || ('\u{A0}'..='\u{FF}').contains(&ch)
        || ('\u{400}'..='\u{4FF}').contains(&ch)
        || CONSOLE_SAFE.contains(&ch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Знаки вне ASCII, которые безопасны в консоли со шрифтом по умолчанию.
    const SAFE_NON_ASCII: &[char] = CONSOLE_SAFE;

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("src dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// Новый знак вне этого модуля не появится: экран, который пишет `✓`, `●`
    /// или `\u{26A0}` в литерале, нарисует квадрат на старой консоли.
    #[test]
    fn no_screen_hardcodes_a_glyph_outside_this_module() {
        let mut files = Vec::new();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        rust_files(&src.join("tui"), &mut files);
        files.push(src.join("app/view_state.rs"));
        let mut offenders = Vec::new();
        for file in files {
            let name = file
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            // Здесь и в фикстурах снимков знаки на месте.
            if name == "glyphs.rs" || name == "capture.rs" {
                continue;
            }
            // Git на Windows отдаёт CRLF, а разделитель ниже записан через LF.
            let text = std::fs::read_to_string(&file)
                .expect("source file")
                .replace("\r\n", "\n");
            // Тесты самих файлов не рисуются: их данные могут быть любыми.
            let code_only = text.split("#[cfg(test)]\nmod ").next().unwrap_or(&text);
            for (number, line) in code_only.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                let code = line.split(" // ").next().unwrap_or(line);
                let mut bad: Vec<String> = Vec::new();
                let mut rest = code;
                while let Some(start) = rest.find("\\u{") {
                    let after = &rest[start + 3..];
                    if let Some(end) = after.find('}')
                        && let Ok(value) = u32::from_str_radix(&after[..end], 16)
                        && let Some(ch) = char::from_u32(value)
                        && !is_console_safe(ch)
                    {
                        bad.push(format!("\\u{{{:X}}}", value));
                    }
                    rest = &rest[start + 3..];
                }
                bad.extend(
                    code.chars()
                        .filter(|ch| {
                            let o = *ch as u32;
                            !is_console_safe(*ch)
                                && ((0x2190..=0x2BFF).contains(&o) || o >= 0x1F000 || o == 0xFE0F)
                        })
                        .map(String::from),
                );
                if !bad.is_empty() {
                    offenders.push(format!("{name}:{}: {bad:?}", number + 1));
                }
            }
        }
        assert!(offenders.is_empty(), "знаки вне glyphs.rs: {offenders:#?}");
    }

    fn all_glyphs(set: &Glyphs) -> Vec<&'static str> {
        let mut all = vec![
            set.check,
            set.cross,
            set.warn,
            set.info,
            set.tool,
            set.clip,
            set.alarm,
            set.brand,
            set.heavy_rule,
            set.bar,
            set.quote_bar,
            set.prompt,
            set.pointer,
            set.checked,
            set.unchecked,
            set.radio_on,
            set.radio_off,
            set.sel_bar,
            set.cooling,
            set.star,
            set.gauge_full,
            set.gauge_est,
            set.gauge_empty,
            set.caret,
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
                assert!(glyph.is_empty() == (glyph == set.brand && set.ascii));
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
