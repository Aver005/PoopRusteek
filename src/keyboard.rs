//! Клавиши не зависят от раскладки. На Windows crossterm берёт букву сочетания из
//! текущей раскладки: на русской Ctrl+A приходит как Ctrl+Ф, а `y` в окне — как `н`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Буквы ЙЦУКЕН (и украинские) и латиница на тех же клавишах QWERTY.
const BY_POSITION: &[(char, char)] = &[
    ('й', 'q'),
    ('ц', 'w'),
    ('у', 'e'),
    ('к', 'r'),
    ('е', 't'),
    ('н', 'y'),
    ('г', 'u'),
    ('ш', 'i'),
    ('щ', 'o'),
    ('з', 'p'),
    ('х', '['),
    ('ъ', ']'),
    ('ї', ']'),
    ('ф', 'a'),
    ('ы', 's'),
    ('і', 's'),
    ('в', 'd'),
    ('а', 'f'),
    ('п', 'g'),
    ('р', 'h'),
    ('о', 'j'),
    ('л', 'k'),
    ('д', 'l'),
    ('ж', ';'),
    ('э', '\''),
    ('є', '\''),
    ('я', 'z'),
    ('ч', 'x'),
    ('с', 'c'),
    ('м', 'v'),
    ('и', 'b'),
    ('т', 'n'),
    ('ь', 'm'),
    ('б', ','),
    ('ю', '.'),
    ('ё', '`'),
];

/// Знак на той же клавише с Shift: `[` → `{` и так далее.
fn shifted(ch: char) -> char {
    match ch {
        '[' => '{',
        ']' => '}',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '`' => '~',
        other => other.to_ascii_uppercase(),
    }
}

/// Латинский знак на той же клавише, что `ch`; `None` — это не кириллица.
pub fn latin_at(ch: char) -> Option<char> {
    let lower = ch.to_lowercase().next()?;
    let (_, latin) = BY_POSITION.iter().find(|(cyr, _)| *cyr == lower)?;
    Some(if lower == ch { *latin } else { shifted(*latin) })
}

/// Клавиша, какой её ждут обработчики. Сочетание с Ctrl или Alt (но не AltGr,
/// это Ctrl+Alt и печать) переводится всегда; простая буква — только если
/// `letters_are_commands`: в поле ввода кириллица остаётся текстом.
pub fn layout_independent(key: KeyEvent, letters_are_commands: bool) -> KeyEvent {
    let KeyCode::Char(ch) = key.code else {
        return key;
    };
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shortcut = ctrl != alt;
    let command = !ctrl && !alt && letters_are_commands;
    match latin_at(ch) {
        Some(latin) if shortcut || command => KeyEvent {
            code: KeyCode::Char(latin),
            ..key
        },
        _ => key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(ch: char, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), modifiers)
    }

    #[test]
    fn ctrl_shortcuts_work_on_the_russian_layout() {
        for (cyr, latin) in [('ф', 'a'), ('и', 'b'), ('м', 'v'), ('д', 'l'), ('с', 'c')] {
            let out = layout_independent(key(cyr, KeyModifiers::CONTROL), false);
            assert_eq!(out.code, KeyCode::Char(latin), "{cyr}");
            assert_eq!(out.modifiers, KeyModifiers::CONTROL);
        }
    }

    #[test]
    fn typed_cyrillic_stays_text_in_a_field() {
        let out = layout_independent(key('н', KeyModifiers::NONE), false);
        assert_eq!(out.code, KeyCode::Char('н'));
        let altgr = layout_independent(key('ф', KeyModifiers::CONTROL | KeyModifiers::ALT), true);
        assert_eq!(
            altgr.code,
            KeyCode::Char('ф'),
            "AltGr prints, it is not a shortcut"
        );
    }

    #[test]
    fn letter_commands_work_where_letters_are_commands() {
        assert_eq!(
            layout_independent(key('н', KeyModifiers::NONE), true).code,
            KeyCode::Char('y')
        );
        assert_eq!(
            layout_independent(key('Н', KeyModifiers::SHIFT), true).code,
            KeyCode::Char('Y')
        );
        assert_eq!(
            layout_independent(key('Х', KeyModifiers::SHIFT), true).code,
            KeyCode::Char('{')
        );
        assert_eq!(
            layout_independent(key('7', KeyModifiers::NONE), true).code,
            KeyCode::Char('7')
        );
    }
}
