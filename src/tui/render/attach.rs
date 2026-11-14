//! Полоса вложений над полем ввода.

use crate::app::AppState;
use crate::tui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

pub(super) fn render_attach_bar(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let files = &state.attached_files;
    if files.is_empty() {
        return;
    }
    let mut spans: Vec<Span> = vec![Span::styled(
        format!(" {} ", crate::tui::glyphs::get().clip),
        Style::default().fg(theme.accent_soft).bg(theme.input_bg),
    )];
    // Много файлов — счётчик вместо ленты имён, которую всё равно обрежет.
    let listed = if files.len() > crate::app::attachments::MAX_LISTED_NAMES {
        let total: u64 = files.iter().map(|f| f.size).sum();
        spans.push(Span::styled(
            format!(
                " {} files {} ",
                files.len(),
                crate::util::format_size(total)
            ),
            Style::default().fg(theme.fg).bg(theme.input_bg),
        ));
        &files[..0]
    } else {
        &files[..]
    };
    let max_w = area.width.saturating_sub(4) as usize;
    let mut remaining = max_w;
    for (i, f) in listed.iter().enumerate() {
        let size_str = crate::util::format_size(f.size);
        let kind = if f.is_image { "img " } else { "" };
        let label = format!(" {kind}{} {} ", f.display_name, size_str);
        let sep = if i > 0 { " " } else { "" };
        let entry = format!("{}{}", sep, label);
        if UnicodeWidthStr::width(entry.as_str()) > remaining {
            if spans.len() > 1 {
                spans.push(Span::styled(
                    " \u{2026}",
                    Style::default().fg(theme.text_dim).bg(theme.input_bg),
                ));
            }
            break;
        }
        remaining = remaining.saturating_sub(UnicodeWidthStr::width(entry.as_str()));
        spans.push(Span::styled(
            entry,
            Style::default().fg(theme.fg).bg(theme.input_bg),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.input_bg)),
        area,
    );
}
