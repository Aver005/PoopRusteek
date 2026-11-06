//! Перенос текста по ширине в клетках терминала. Чистые функции без
//! рисования: их зовут и отрисовка, и обработчики клавиш, чтобы число
//! строк совпадало с тем, что видит человек.

use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

/// Ширина символа в клетках; нулевую ширину (комбинирующие знаки) не округляем вверх.
fn cell_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Перенос по словам с жёстким разрезом слишком длинных слов. Явные `\n`
/// сохраняются, пустая строка остаётся пустой строкой.
pub fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        let mut row = String::new();
        let mut used = 0usize;
        for word in paragraph.split(' ').filter(|word| !word.is_empty()) {
            let word_width: usize = word.chars().map(cell_width).sum();
            let gap = usize::from(used > 0);
            if used > 0 && used + gap + word_width > width {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            if word_width > width {
                for ch in word.chars() {
                    let w = cell_width(ch);
                    if used + w > width && used > 0 {
                        rows.push(std::mem::take(&mut row));
                        used = 0;
                    }
                    row.push(ch);
                    used += w;
                }
                continue;
            }
            if used > 0 {
                row.push(' ');
                used += 1;
            }
            row.push_str(word);
            used += word_width;
        }
        rows.push(row);
    }
    rows
}

/// Перенос строки из цветных кусков: предпочитает границу слова (после
/// пробела), а слово без пробелов режет ровно по ширине. Пробелы остаются на
/// концах строк — ни один символ не теряется. Продолжение начинается с `hang`
/// пробелов (не больше половины ширины).
pub fn wrap_spans(spans: &[Span<'static>], width: usize, hang: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let hang = hang.min(width / 2);
    let indent: Vec<(char, Style)> = vec![(' ', Style::default()); hang];
    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut row: Vec<(char, Style)> = Vec::new();
    let mut used = 0usize;
    // Сколько ячеек в начале строки — отступ продолжения (резать там нечего).
    let mut floor = 0usize;
    for span in spans {
        for ch in span.content.chars() {
            let w = cell_width(ch);
            // `while`: перенесённый хвост вместе с отступом тоже мог не влезть.
            while used + w > width && row.len() > floor {
                // Граница слова: после последнего пробела, но не слишком рано —
                // иначе строка получилась бы почти пустой.
                let cut = row
                    .iter()
                    .rposition(|(c, _)| *c == ' ')
                    .filter(|index| *index >= floor && row_width(&row[..=*index]) * 3 >= width);
                let carry: Vec<(char, Style)> = match cut {
                    Some(index) if index + 1 < row.len() => row.split_off(index + 1),
                    _ => Vec::new(),
                };
                rows.push(std::mem::take(&mut row));
                row = indent.clone();
                floor = hang;
                row.extend(carry);
                used = row_width(&row);
            }
            row.push((ch, span.style));
            used += w;
        }
    }
    rows.push(row);
    rows.into_iter().map(group_cells).collect()
}

fn row_width(cells: &[(char, Style)]) -> usize {
    cells.iter().map(|(c, _)| cell_width(*c)).sum()
}

/// Подряд идущие клетки одного стиля — один кусок.
fn group_cells(cells: Vec<(char, Style)>) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut style = None;
    for (ch, cell_style) in cells {
        if style.is_some_and(|current| current != cell_style) {
            if let Some(current) = style {
                spans.push(Span::styled(std::mem::take(&mut text), current));
            }
            style = None;
        }
        style.get_or_insert(cell_style);
        text.push(ch);
    }
    if let Some(current) = style {
        spans.push(Span::styled(text, current));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn words_wrap_at_the_width_and_keep_every_word() {
        let rows = wrap_words("aaa bbb ccc ddd", 7);
        assert_eq!(rows, vec!["aaa bbb", "ccc ddd"]);
    }

    #[test]
    fn a_word_longer_than_the_width_is_cut_not_dropped() {
        let rows = wrap_words("abcdefghij", 4);
        assert_eq!(rows, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn explicit_newlines_and_empty_lines_survive() {
        assert_eq!(wrap_words("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn wide_characters_count_two_cells() {
        for row in wrap_words("你好世界你好世界你好", 6) {
            assert!(UnicodeWidthStr::width(row.as_str()) <= 6, "{row}");
        }
    }

    #[test]
    fn zero_width_is_treated_as_one_cell() {
        assert!(!wrap_words("abc", 0).is_empty());
    }

    #[test]
    fn spans_wrap_without_losing_a_character() {
        let style = Style::default();
        let text = "x".repeat(50);
        let rows = wrap_spans(&[Span::styled(text.clone(), style)], 12, 2);
        let rows = text_of(&rows);
        let joined: String = rows
            .iter()
            .enumerate()
            .map(|(index, row)| if index == 0 { row.as_str() } else { &row[2..] })
            .collect();
        assert_eq!(joined, text);
        for row in &rows {
            assert!(UnicodeWidthStr::width(row.as_str()) <= 12, "{row}");
        }
    }

    #[test]
    fn spans_keep_their_own_styles_across_a_cut() {
        let red = Style::default().fg(ratatui::style::Color::Red);
        let rows = wrap_spans(&[Span::styled("aaaaaa", red), Span::raw("bbbbbb")], 8, 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].style, red);
    }

    fn text_of(rows: &[Vec<Span<'static>>]) -> Vec<String> {
        rows.iter()
            .map(|row| row.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn spans_prefer_a_word_boundary_and_keep_the_space() {
        let rows = wrap_spans(&[Span::raw("hello world foo bar")], 12, 0);
        let rows = text_of(&rows);
        assert_eq!(rows, vec!["hello world ", "foo bar"]);
        assert_eq!(rows.concat(), "hello world foo bar", "ничего не потеряно");
    }

    #[test]
    fn a_word_longer_than_the_row_is_still_cut_by_width() {
        let rows = wrap_spans(&[Span::raw("aaaaaaaaaaaaaaaaaaaa")], 8, 0);
        let rows = text_of(&rows);
        assert!(rows.len() >= 3);
        assert_eq!(rows.concat(), "aaaaaaaaaaaaaaaaaaaa");
        for row in &rows {
            assert!(UnicodeWidthStr::width(row.as_str()) <= 8, "{row}");
        }
    }

    #[test]
    fn a_break_that_would_leave_a_nearly_empty_row_is_not_taken() {
        // Пробел в самом начале: резать после него — почти пустая строка, режем по ширине.
        let rows = text_of(&wrap_spans(&[Span::raw("a bbbbbbbbbbbbbbbbbbbb")], 8, 0));
        assert_eq!(rows[0].chars().count(), 8, "{rows:?}");
        assert_eq!(rows.concat(), "a bbbbbbbbbbbbbbbbbbbb");
    }

    #[test]
    fn a_continuation_keeps_its_hang_and_every_character() {
        let source = "command: cargo test --bin x items::range -- --nocapture && git diff";
        let rows = text_of(&wrap_spans(&[Span::raw(source)], 24, 4));
        let joined: String = rows
            .iter()
            .enumerate()
            .map(|(index, row)| if index == 0 { row.as_str() } else { &row[4..] })
            .collect();
        assert_eq!(joined, source);
        for row in rows.iter().skip(1) {
            assert!(row.starts_with("    "), "{row:?}");
        }
    }
}
