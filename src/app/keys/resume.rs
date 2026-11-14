//! Клавиши экрана `/resume`: стрелки и страницы ходят по списку, набор правит
//! фильтр, Tab — этот проект или все, Enter открывает, Esc возвращает в чат.

use super::apply_text_key;
use crate::app::App;
use crate::app::events::{AppEvent, View};
use crate::app::list::ListNav;
use crate::error::AppResult;

impl App {
    /// Открыть экран и прочитать сессии в фоне: цикл событий диск не ждёт.
    pub(crate) fn open_resume(&mut self) {
        self.state.resume.open();
        self.state.view = View::Resume;
        let event_tx = self.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let sessions = crate::session::list_sessions().unwrap_or_default();
            let _ = event_tx.send(AppEvent::ResumeSessionsLoaded(sessions));
        });
    }

    pub(crate) fn on_resume_loaded(&mut self, sessions: Vec<crate::session::SessionSummary>) {
        self.state.resume.apply(sessions);
        self.refilter_resume();
    }

    /// Список сменился (фильтр, выборка, чтение): курсор — в его границы.
    pub(crate) fn refilter_resume(&mut self) {
        let len = self.state.resume.visible(&self.state.workspace_path).len();
        let page =
            crate::app::list::page_rows(self.state.terminal_rows, crate::app::list::rows::SESSION);
        self.state.resume.refilter(len, page);
    }

    pub(super) async fn handle_resume_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> AppResult<bool> {
        use crossterm::event::KeyCode;
        let len = self.state.resume.visible(&self.state.workspace_path).len();
        let page =
            crate::app::list::page_rows(self.state.terminal_rows, crate::app::list::rows::SESSION);
        if let Some(nav) = ListNav::from_code(key.code) {
            self.state.resume.navigate(nav, len, page);
            return Ok(false);
        }
        match key.code {
            KeyCode::Esc => self.state.view = View::Chat,
            KeyCode::Tab => {
                self.state.resume.toggle_scope();
                self.refilter_resume();
            }
            KeyCode::Enter => {
                if let Some(id) = self.state.resume.selected_id(&self.state.workspace_path) {
                    self.state.view = View::Chat;
                    if let Err(error) = self.handle_load_session(&id).await {
                        self.state.status_message = format!("Cannot open the session: {error}");
                    }
                }
            }
            _ => {
                if apply_text_key(&mut self.state.resume.filter, key, false) {
                    self.refilter_resume();
                }
            }
        }
        Ok(false)
    }
}
