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
}

impl LandingSessions {
    /// Сессии для показа: самые свежие `SHOWN_SESSIONS`.
    pub fn recent(&self) -> &[SessionSummary] {
        &self.sessions[..self.sessions.len().min(SHOWN_SESSIONS)]
    }

    /// Положить прочитанный список. Зовёт цикл событий, когда пришёл ответ.
    pub fn apply(&mut self, sessions: Vec<SessionSummary>) {
        self.sessions = sessions;
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
}
