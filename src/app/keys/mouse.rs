//! Mouse handling: the wheel scrolls whatever window is focused. It is
//! deliberately a *view scroll* only — history recall stays on Ctrl+Up/Down
//! (see [`chat`](super::chat)), so the wheel never fights the input history.
//!
//! Each surface scrolls in the same direction its own Up/Down keys do:
//! - chat transcript — `scroll_offset` counts rows from the bottom, so wheel
//!   up increases it (older messages),
//! - MCP details / tool-approval modal — `scroll_offset` counts from the top,
//!   so wheel up decreases it.

use crate::app::App;
use crate::app::events::{Modal, View};
use crate::app::list::{ListNav, page_rows, rows};

/// Rows moved per wheel notch — a little faster than a single arrow press.
const WHEEL_STEP: usize = 3;

impl App {
    pub(crate) fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;
        let up = match mouse.kind {
            MouseEventKind::ScrollUp => true,
            MouseEventKind::ScrollDown => false,
            // Clicks, drags and moves are not wired to anything yet.
            _ => return,
        };

        // A modal owns the screen when present — scroll it, not the chat behind.
        if let Some(Modal::ToolApproval {
            arguments,
            scroll_offset,
            ..
        }) = self.state.modal.as_mut()
        {
            if up {
                *scroll_offset = scroll_offset.saturating_sub(WHEEL_STEP);
            } else {
                // Mirror the key handler's clamp so the wheel can't scroll past
                // the end of the argument block.
                let max_scroll = arguments
                    .lines()
                    .count()
                    .saturating_sub(super::modal::TOOL_APPROVAL_VISIBLE_LINES);
                *scroll_offset = (*scroll_offset + WHEEL_STEP).min(max_scroll);
            }
            return;
        }
        if self.state.modal.is_some() {
            return;
        }

        // Списки прокручиваются курсором — тем же перемещением, что и j/k.
        let nav = if up { ListNav::Up } else { ListNav::Down };
        let page = |per_item| page_rows(self.state.terminal_rows, per_item);
        match self.state.view {
            View::Mcp => {
                let view = &mut self.state.mcp_status.view;
                if view.details_server.is_some() {
                    view.details_scroll = if up {
                        view.details_scroll.saturating_sub(1)
                    } else {
                        view.details_scroll.saturating_add(1)
                    };
                } else {
                    let len = view.visible_indices().len();
                    view.selected = nav.move_within(view.selected, len, page(rows::SERVER));
                }
            }
            View::Search => {
                let len = self.state.search.visible().len();
                let search = &mut self.state.search;
                search.selected = nav.move_within(search.selected, len, page(rows::MATCH));
            }
            View::Themes => {
                let len = crate::app::themes::theme_rows(&self.config).len();
                let selected = self.state.themes.selected;
                self.state.themes.selected = nav.move_within(selected, len, page(rows::THEME));
            }
            View::Providers => {
                let len = crate::app::providers::provider_rows(&self.config).len();
                let selected = self.state.providers_view.selected;
                self.state.providers_view.selected =
                    nav.move_within(selected, len, page(rows::PROVIDER));
            }
            _ => {
                // Chat transcript (the default view). scroll_offset is u32.
                let step = WHEEL_STEP as u32;
                self.state.scroll_offset = if up {
                    self.state.scroll_offset.saturating_add(step)
                } else {
                    self.state.scroll_offset.saturating_sub(step)
                };
            }
        }
    }
}
