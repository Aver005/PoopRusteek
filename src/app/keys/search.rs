//! Keys for the `/search` screen. Two focus zones: the query line owns
//! plain characters (text editing via the shared `apply_text_key`), the
//! result list owns navigation and single-letter actions — so `s`/`r`/`u`
//! can be hotkeys without fighting the input field.

use super::apply_text_key;
use crate::app::App;
use crate::app::events::View;
use crate::app::search::SearchFocus;
use crate::error::AppResult;

impl App {
    pub(super) async fn handle_search_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> AppResult<bool> {
        use crossterm::event::KeyCode;

        match self.state.search.focus {
            SearchFocus::Query => match key.code {
                KeyCode::Esc => {
                    self.state.view = View::Chat;
                }
                KeyCode::Enter => {
                    let query = self.state.search.query.buffer.trim().to_string();
                    if !query.is_empty() {
                        self.spawn_history_search(query);
                    }
                }
                KeyCode::Down | KeyCode::Tab if !self.state.search.visible().is_empty() => {
                    self.state.search.focus = SearchFocus::Results;
                }
                _ => {
                    apply_text_key(&mut self.state.search.query, key, false);
                }
            },
            SearchFocus::Results => {
                let visible = self.state.search.visible();
                let max = visible.len().saturating_sub(1);
                let search = &mut self.state.search;
                search.selected = search.selected.min(max);

                match key.code {
                    KeyCode::Esc | KeyCode::Tab => {
                        search.focus = SearchFocus::Query;
                    }
                    KeyCode::Char('q') => {
                        self.state.view = View::Chat;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        if search.selected == 0 {
                            // Off the top of the list = back to the query.
                            search.focus = SearchFocus::Query;
                        } else {
                            search.selected -= 1;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        search.selected = (search.selected + 1).min(max);
                    }
                    KeyCode::Home | KeyCode::Char('g') => search.selected = 0,
                    KeyCode::End | KeyCode::Char('G') => search.selected = max,
                    KeyCode::Char('s') => {
                        search.sort = search.sort.next();
                        search.reset_selection();
                    }
                    KeyCode::Char('r') => {
                        search.role_filter = search.role_filter.next();
                        search.reset_selection();
                        if search.visible().is_empty() {
                            search.focus = SearchFocus::Query;
                        }
                    }
                    KeyCode::Char('u') => {
                        search.unique_sessions = !search.unique_sessions;
                        search.reset_selection();
                    }
                    KeyCode::Enter => {
                        if let Some(&index) = visible.get(search.selected) {
                            let session_id = search.matches[index].session_id.clone();
                            self.state.view = View::Chat;
                            self.handle_load_session(&session_id).await?;
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(false)
    }
}
