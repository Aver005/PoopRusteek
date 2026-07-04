//! Top-level frame assembly: routes to the chat view, the landing screen,
//! the onboarding screen, or the MCP management view, plus modals and the
//! status bar.
//!
//! Layout of this module tree, bottom layer first:
//! - [`util`] — pure text/format/layout helpers (no `AppState`, no drawing)
//! - [`popup`] — the shared popup/modal skeleton every modal builds on
//! - [`status`], [`landing`], [`onboarding`], [`mcp`], [`modals`] — the
//!   actual views; [`render`] below only dispatches between them.

mod landing;
mod mcp;
mod modals;
mod onboarding;
mod popup;
mod providers;
mod status;
mod util;

use crate::app::AppState;
use crate::app::events::View;
use crate::config::Config;
use crate::tui::TuiTerminal;
use crate::tui::theme::Theme;
use crate::tui::widgets;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::Paragraph;
use std::cell::{Cell, RefCell};

const BRAND_TITLE: &str = "PoopRusteek \u{1F9FB}";

thread_local! {
    /// Last title written to the terminal — OSC title changes are emitted
    /// only on change, not on every frame.
    static LAST_WINDOW_TITLE: RefCell<String> = const { RefCell::new(String::new()) };
}

/// The window title for the current app state: the brand on the landing /
/// onboarding / management screens, a truncated chat title (from the
/// chat's first real message) once a conversation has content.
fn window_title(state: &AppState) -> String {
    if state.view != View::Chat {
        return BRAND_TITLE.to_string();
    }
    let title = state
        .focused()
        .messages
        .iter()
        .find(|message| {
            !message.ui_only
                && matches!(
                    message.role,
                    crate::provider::Role::User | crate::provider::Role::Assistant
                )
                && !message.visible_content().trim().is_empty()
        })
        .map(|message| {
            let first_line = message
                .visible_content()
                .trim()
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            util::truncate(&first_line, 40)
        });
    match title {
        Some(title) if !title.is_empty() => title,
        _ => BRAND_TITLE.to_string(),
    }
}

fn sync_window_title(terminal: &mut TuiTerminal, state: &AppState) {
    let title = window_title(state);
    LAST_WINDOW_TITLE.with(|last| {
        let mut last = last.borrow_mut();
        if *last != title {
            let _ = crossterm::execute!(
                terminal.backend_mut(),
                crossterm::terminal::SetTitle(&title)
            );
            *last = title;
        }
    });
}

pub fn render(terminal: &mut TuiTerminal, state: &AppState, config: &Config) -> crate::error::AppResult<()> {
    let theme = Theme::default_dark();
    let cursor_cell = Cell::new(None);
    sync_window_title(terminal, state);
    terminal.draw(|frame| {
        let area = frame.area();
        frame.render_widget(
            Paragraph::new(String::new()).style(Style::default().bg(theme.bg)),
            area,
        );

        if state.view == View::Onboarding {
            onboarding::render_onboarding(frame, area, &state.onboarding, state.focused().generation.animation_tick, &theme, &cursor_cell);
            // Bottom status line — /logout and /wipe land here with their result.
            if area.height > 1 {
                let status_area = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
                status::render_mini_status(frame, status_area, state, config, &theme);
            }
        } else if state.view == View::Mcp {
            mcp::render_mcp_view(frame, area, &state.mcp_status.view, &theme);
        } else if state.view == View::Providers {
            providers::render_providers_view(frame, area, &state.providers_view, config, &theme);
        } else if state.focused().messages.is_empty() && !state.focused().generation.active {
            landing::render_landing(frame, area, state, config, &theme, &cursor_cell);
        } else {
            let has_attachments = !state.attached_files.is_empty();
            let mut constraints = vec![
                Constraint::Min(1),
                Constraint::Length(1),
            ];
            if has_attachments {
                constraints.push(Constraint::Length(1));
            }
            constraints.push(Constraint::Length(4));
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(1));

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);

            status::render_separator(frame, chunks[1], &theme);

            // Split main content into chat + stats panel
            let pad_w = 2u16;
            if state.show_stats_panel {
                let panel_w = 34u16.min(chunks[0].width / 4);
                let content_chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(pad_w),
                        Constraint::Min(1),
                        Constraint::Length(panel_w),
                    ])
                    .split(chunks[0]);
                widgets::chat::render_chat(frame, content_chunks[1], state, &theme);
                widgets::panel::render_stats_panel(frame, chunks[0], state, config, &theme);
            } else {
                let content_chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(pad_w),
                        Constraint::Min(1),
                        Constraint::Length(pad_w),
                    ])
                    .split(chunks[0]);
                widgets::chat::render_chat(frame, content_chunks[1], state, &theme);
            }

            let attach_idx = 2usize;
            let input_idx = if has_attachments { 3usize } else { 2usize };
            let border_idx = if has_attachments { 4usize } else { 3usize };
            let status_idx = if has_attachments { 5usize } else { 4usize };

            if has_attachments {
                status::render_attach_bar(frame, chunks[attach_idx], state, &theme);
            }
            widgets::input::render_input(frame, chunks[input_idx], state, &theme, false, &cursor_cell);
            status::render_input_border(frame, chunks[border_idx], &theme);
            status::render_mini_status(frame, chunks[status_idx], state, config, &theme);

            if state.autocomplete.visible {
                modals::render_autocomplete(frame, chunks[2], state, &theme);
            }
        }

        if let Some(modal) = &state.modal {
            modals::render_modal(frame, area, modal, &theme);
        }
    })?;
    // Re-set cursor position after frame flush to prevent flicker onto (0,0)
    if let Some((cx, cy)) = cursor_cell.get() {
        use crossterm::cursor::MoveTo;
        crossterm::execute!(terminal.backend_mut(), MoveTo(cx, cy))?;
    }
    if state.modal.is_some()
        || state.focused().generation.active
        || state.view == View::Mcp
        || state.view == View::Providers
    {
        terminal.hide_cursor()?;
    } else {
        // For the onboarding view the cursor is positioned by render_onboarding
        // into cursor_cell — show_cursor makes it visible; hide is handled above.
        terminal.show_cursor()?;
    }
    Ok(())
}
