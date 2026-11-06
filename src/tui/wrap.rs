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

/// Жёсткий перенос строки из цветных кусков: режет ровно по ширине, чтобы
/// ни один символ команды не потерялся. Продолжение начинается с `hang`
/// пробелов (не больше половины ширины).
pub fn wrap_spans(spans: &[Span<'static>], width: usize, hang: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let hang = hang.min(width / 2);
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0usize;
    for span in spans {
        let mut chunk = String::new();
        for ch in span.content.chars() {
            let w = cell_width(ch);
            if used + w > width && used > hang_floor(rows.len(), hang) {
                flush_chunk(rows.last_mut(), &mut chunk, span.style);
                rows.push(vec![Span::raw(" ".repeat(hang))]);
                used = hang;
            }
            chunk.push(ch);
            used += w;
        }
        flush_chunk(rows.last_mut(), &mut chunk, span.style);
    }
    rows
}

/// На продолжении строки отступ уже занял место: новый разрез допустим, только если после него что-то есть.
fn hang_floor(row_count: usize, hang: usize) -> usize {
    if row_count > 1 { hang } else { 0 }
}

fn flush_chunk(row: Option<&mut Vec<Span<'static>>>, chunk: &mut String, style: Style) {
    if chunk.is_empty() {
        return;
    }
    if let Some(row) = row {
        row.push(Span::styled(std::mem::take(chunk), style));
    }
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
        let joined: String = rows
            .iter()
            .flat_map(|row| row.iter().map(|s| s.content.as_ref()))
            .filter(|s| !s.trim().is_empty())
            .collect();
        assert_eq!(joined, text);
        for row in &rows {
            let width: usize = row
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(width <= 12, "row is {width} cells");
        }
    }

    #[test]
    fn spans_keep_their_own_styles_across_a_cut() {
        let red = Style::default().fg(ratatui::style::Color::Red);
        let rows = wrap_spans(&[Span::styled("aaaaaa", red), Span::raw("bbbbbb")], 8, 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].style, red);
    }
}
