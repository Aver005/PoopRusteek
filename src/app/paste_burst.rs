//! Вставка на Windows приходит потоком нажатий (bracketed paste в crossterm там нет):
//! серию, пришедшую разом, склеиваем обратно во вставку, чтобы Enter в ней не отправлял.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use std::time::{Duration, Instant};

/// Со скольких знаков серия считается вставкой. Путь к файлу почти всегда длиннее,
/// а два-три знака, накопившихся, пока интерфейс был занят, — это набор.
pub const MIN_PASTE_CHARS: usize = 8;

/// Пауза между нажатиями, после которой серия кончилась. Человек между
/// клавишами тратит десятки миллисекунд, консоль отдаёт вставку быстрее.
pub const GAP: Duration = Duration::from_millis(25);

/// Дольше одной пачки не собираем: интерфейс не должен замирать. Хвост длинной
/// вставки подхватит [`FOLLOW_ON`].
pub const MAX_GATHER: Duration = Duration::from_millis(500);

/// После вставки её хвост, пришедший отдельным коротким куском (`}` и Enter),
/// — всё ещё вставка, а не набор с отправкой.
const FOLLOW_ON: Duration = Duration::from_millis(100);

/// Знак, который нажатие вводит, если это ввод текста. AltGr на Windows — это
/// Ctrl+Alt (`@` на многих раскладках); Enter с любым модификатором — перевод строки.
fn typed_char(key: &KeyEvent) -> Option<char> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) if (!ctrl && !alt) || (ctrl && alt) => Some(c),
        KeyCode::Enter => Some('\n'),
        KeyCode::Tab if key.modifiers.is_empty() => Some('\t'),
        _ => None,
    }
}

/// Нажатие, вводящее текст: только оно начинает и продлевает серию.
pub fn is_text_press(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind == KeyEventKind::Press && typed_char(key).is_some())
}

/// Шум, который серию не рвёт: мышь ведут и фокус меняют прямо во время вставки.
fn is_noise(event: &Event) -> bool {
    match event {
        Event::Key(key) => key.kind != KeyEventKind::Press,
        Event::Mouse(mouse) => mouse.kind == MouseEventKind::Moved,
        Event::FocusGained | Event::FocusLost => true,
        _ => false,
    }
}

/// Помнит, что только что была вставка: её хвост склеивается и короче порога.
#[derive(Default)]
pub struct Burst {
    paste_until: Option<Instant>,
}

impl Burst {
    /// Каждая серия текстовых нажатий от [`MIN_PASTE_CHARS`] → `Event::Paste`.
    /// `arrived` — начало пачки, `gathered` — конец сбора: от него и отсчитывается хвост.
    pub fn coalesce(
        &mut self,
        batch: Vec<Event>,
        arrived: Instant,
        gathered: Instant,
    ) -> Vec<Event> {
        let continuing = self.paste_until.is_some_and(|until| arrived < until);
        let out = coalesce(batch, continuing);
        if out.iter().any(|e| matches!(e, Event::Paste(_))) {
            self.paste_until = Some(gathered + FOLLOW_ON);
        }
        out
    }
}

/// Накопленная серия текстовых нажатий.
#[derive(Default)]
struct Run {
    keys: Vec<Event>,
    text: String,
}

impl Run {
    /// Серия от порога (или хвост недавней вставки) — вставка, короче — нажатия.
    fn flush(&mut self, as_paste_anyway: bool, out: &mut Vec<Event>) {
        if self.keys.is_empty() {
            return;
        }
        if self.text.chars().count() >= MIN_PASTE_CHARS || as_paste_anyway {
            out.push(Event::Paste(std::mem::take(&mut self.text)));
            self.keys.clear();
        } else {
            out.append(&mut self.keys);
            self.text.clear();
        }
    }
}

/// `continuing` — пачка продолжает недавнюю вставку: её первая серия склеивается
/// при любой длине.
fn coalesce(batch: Vec<Event>, continuing: bool) -> Vec<Event> {
    let mut out = Vec::with_capacity(batch.len());
    let mut run = Run::default();
    let mut first_run = true;
    for event in batch {
        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Press
            && let Some(c) = typed_char(key)
        {
            run.text.push(c);
            run.keys.push(event);
            continue;
        }
        if is_noise(&event) {
            // Внутри серии шум выбрасывается, вне её — идёт как был.
            if run.keys.is_empty() {
                out.push(event);
            }
            continue;
        }
        run.flush(continuing && first_run, &mut out);
        first_run = false;
        out.push(event);
    }
    run.flush(continuing && first_run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::MouseEvent;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn release(code: KeyCode) -> Event {
        let mut key = KeyEvent::new(code, KeyModifiers::NONE);
        key.kind = KeyEventKind::Release;
        Event::Key(key)
    }

    fn typed(text: &str) -> Vec<Event> {
        text.chars()
            .flat_map(|c| {
                let code = if c == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(c)
                };
                [press(code, KeyModifiers::NONE), release(code)]
            })
            .collect()
    }

    fn presses(text: &str) -> Vec<Event> {
        typed(text).into_iter().filter(|e| !is_noise(e)).collect()
    }

    fn paste(text: &str) -> Event {
        Event::Paste(text.to_string())
    }

    fn mouse(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// Перетащенный путь и многострочная вставка — одна вставка, и Enter
    /// внутри неё больше не отправляет сообщение.
    #[test]
    fn a_fast_burst_becomes_one_paste() {
        let path = r#""C:\Users\me\My Docs\report.pdf""#;
        assert_eq!(coalesce(typed(path), false), vec![paste(path)]);
        assert_eq!(
            coalesce(typed("line one\nline two"), false),
            vec![paste("line one\nline two")]
        );
    }

    /// Обратное направление: «ок» с Enter, накопившиеся за опрос, — это набор,
    /// Enter по-прежнему отправляет.
    #[test]
    fn a_short_burst_stays_keystrokes() {
        assert_eq!(coalesce(typed("ok\n"), false), presses("ok\n"));
    }

    /// Движение мыши посреди вставки её не рвёт, а Esc — отдельное событие
    /// между вставками, а не повод раздать их нажатиями.
    #[test]
    fn noise_does_not_break_a_paste_and_other_keys_pass_through_in_order() {
        let mut batch = typed("first line\n");
        batch.push(mouse(MouseEventKind::Moved));
        batch.extend(typed("second line"));
        batch.push(press(KeyCode::Esc, KeyModifiers::NONE));
        batch.extend(typed("tail line one"));
        assert_eq!(
            coalesce(batch, false),
            vec![
                paste("first line\nsecond line"),
                press(KeyCode::Esc, KeyModifiers::NONE),
                paste("tail line one"),
            ]
        );
    }

    #[test]
    fn a_control_key_splits_runs_and_short_runs_stay_keys() {
        let mut batch = presses("ab");
        batch.push(press(KeyCode::Char('c'), KeyModifiers::CONTROL));
        batch.extend(presses("abcdefghij"));
        let mut expected = presses("ab");
        expected.push(press(KeyCode::Char('c'), KeyModifiers::CONTROL));
        expected.push(paste("abcdefghij"));
        assert_eq!(coalesce(batch, false), expected);
    }

    /// Хвост вставки отдельным коротким куском (`}` и Enter) — всё ещё вставка.
    #[test]
    fn the_short_tail_of_a_paste_still_pastes() {
        let mut burst = Burst::default();
        let now = Instant::now();
        assert_eq!(
            burst.coalesce(typed("fn main() {\n"), now, now),
            vec![paste("fn main() {\n")]
        );
        let then = now + Duration::from_millis(30);
        let tail = burst.coalesce(typed("}\n"), then, then);
        assert_eq!(tail, vec![paste("}\n")]);
        // А через секунду это снова набор.
        let later_at = now + Duration::from_secs(1);
        let later = burst.coalesce(typed("ok\n"), later_at, later_at);
        assert_eq!(later, presses("ok\n"));
    }

    #[test]
    fn altgr_and_modified_enter_are_text() {
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        let mut batch = presses("user");
        batch.push(press(KeyCode::Char('@'), altgr));
        batch.extend(presses("host"));
        batch.push(press(KeyCode::Enter, KeyModifiers::CONTROL));
        assert_eq!(coalesce(batch, false), vec![paste("user@host\n")]);
        assert!(is_text_press(&press(KeyCode::Char('@'), altgr)));
        assert!(!is_text_press(&press(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
    }
}
