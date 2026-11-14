//! Список недавних сессий для главного экрана. Диск читает фоновая задача,
//! а отрисовка берёт готовое из состояния и больше ничего не делает.

use crate::app::events::AppEvent;
use crate::session::SessionSummary;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Через сколько список считается устаревшим и читается заново.
const REFRESH_AFTER: Duration = Duration::from_secs(3);

/// Сколько сессий главная показывает.
pub const SHOWN_SESSIONS: usize = 5;

#[derive(Debug, Default)]
pub struct LandingSessions {
    sessions: Vec<SessionSummary>,
    fetched_at: Option<Instant>,
    loading: bool,
    /// Выбранная курсором сессия; `None` — курсор не на списке, ввод свободен.
    cursor: Option<usize>,
}

impl LandingSessions {
    /// Сессии для показа: самые свежие `SHOWN_SESSIONS`.
    pub fn recent(&self) -> &[SessionSummary] {
        &self.sessions[..self.sessions.len().min(SHOWN_SESSIONS)]
    }

    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    /// Вниз: из пустоты на первую строку, дальше до последней из `visible` —
    /// тех, что главная сейчас нарисовала. Невидимую сессию выбрать нельзя.
    pub fn step_down(&mut self, visible: usize) {
        let last = self.recent().len().min(visible).checked_sub(1);
        self.cursor = match (self.cursor, last) {
            (_, None) => None,
            (None, Some(_)) => Some(0),
            (Some(at), Some(last)) => Some((at + 1).min(last)),
        };
    }

    /// Вверх: с первой строки курсор уходит со списка.
    pub fn step_up(&mut self) {
        self.cursor = match self.cursor {
            Some(at) if at > 0 => Some(at - 1),
            _ => None,
        };
    }

    /// Поставить курсор на сессию номер `index` (с нуля), если она нарисована.
    pub fn pick(&mut self, index: usize, visible: usize) -> bool {
        let ok = index < self.recent().len().min(visible);
        if ok {
            self.cursor = Some(index);
        }
        ok
    }

    pub fn clear_cursor(&mut self) {
        self.cursor = None;
    }

    /// Id выбранной сессии.
    pub fn selected_id(&self) -> Option<String> {
        self.recent().get(self.cursor?).map(|s| s.id.clone())
    }

    /// Положить прочитанный список. Зовёт цикл событий, когда пришёл ответ.
    pub fn apply(&mut self, sessions: Vec<SessionSummary>) {
        self.sessions = sessions;
        // Список сократился — курсор не должен висеть за его концом.
        if self.cursor.is_some_and(|at| at >= self.recent().len()) {
            self.cursor = None;
        }
        self.fetched_at = Some(Instant::now());
        self.loading = false;
    }

    /// Пора ли читать заново: не идёт чтение и список старый или не читался.
    fn is_stale(&self) -> bool {
        !self.loading
            && self
                .fetched_at
                .is_none_or(|at| at.elapsed() >= REFRESH_AFTER)
    }

    /// Запустить чтение, если список устарел. Диск читает `spawn_blocking`.
    pub fn refresh_if_stale(&mut self, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        if !self.is_stale() {
            return;
        }
        self.loading = true;
        let event_tx = event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let sessions = crate::session::list_sessions().unwrap_or_default();
            let _ = event_tx.send(AppEvent::LandingSessionsLoaded(sessions));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            workspace_root: String::new(),
            message_count: 0,
            title: String::new(),
            model_type: String::new(),
            tag: None,
            broken: false,
        }
    }

    #[test]
    fn a_fresh_state_wants_a_read() {
        assert!(LandingSessions::default().is_stale());
    }

    #[test]
    fn a_read_in_flight_is_not_started_twice() {
        let mut landing = LandingSessions {
            loading: true,
            ..Default::default()
        };
        assert!(!landing.is_stale());
        landing.apply(vec![summary("a")]);
        assert!(!landing.is_stale(), "только что прочитан — ещё свежий");
    }

    #[test]
    fn only_the_latest_few_are_shown() {
        let mut landing = LandingSessions::default();
        landing.apply((0..9).map(|n| summary(&n.to_string())).collect());
        assert_eq!(landing.recent().len(), SHOWN_SESSIONS);
        assert_eq!(landing.recent()[0].id, "0");
    }

    #[test]
    fn the_cursor_walks_the_list_and_leaves_it_from_the_top() {
        let mut landing = LandingSessions::default();
        landing.apply(vec![summary("a"), summary("b")]);
        assert_eq!(landing.cursor(), None);
        landing.step_down(SHOWN_SESSIONS);
        assert_eq!(landing.selected_id().as_deref(), Some("a"));
        landing.step_down(SHOWN_SESSIONS);
        landing.step_down(SHOWN_SESSIONS);
        assert_eq!(
            landing.selected_id().as_deref(),
            Some("b"),
            "stops at the last row"
        );
        landing.step_up();
        landing.step_up();
        assert_eq!(
            landing.cursor(),
            None,
            "up from the first row frees the input"
        );
    }

    #[test]
    fn the_cursor_never_goes_past_what_is_drawn() {
        let mut landing = LandingSessions::default();
        landing.apply(vec![summary("a"), summary("b"), summary("c")]);
        for _ in 0..5 {
            landing.step_down(2);
        }
        assert_eq!(landing.selected_id().as_deref(), Some("b"));
        let mut hidden = LandingSessions::default();
        hidden.apply(vec![summary("a")]);
        hidden.step_down(0);
        assert_eq!(hidden.cursor(), None, "nothing drawn, nothing to pick");
        assert!(!hidden.pick(0, 0));
        assert!(hidden.pick(0, 1));
        assert_eq!(hidden.selected_id().as_deref(), Some("a"));
    }

    #[test]
    fn a_shorter_list_never_leaves_the_cursor_past_its_end() {
        let mut landing = LandingSessions::default();
        landing.apply(vec![summary("a"), summary("b"), summary("c")]);
        landing.step_down(SHOWN_SESSIONS);
        landing.step_down(SHOWN_SESSIONS);
        landing.step_down(SHOWN_SESSIONS);
        landing.apply(vec![summary("a")]);
        assert_eq!(landing.cursor(), None);
    }
}
