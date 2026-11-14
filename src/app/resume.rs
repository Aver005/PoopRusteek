//! `/resume` (и `/sessions`) — выбор сессии на весь экран. Диск читает фоновая
//! задача; здесь только список, фильтр, проектная выборка и курсор с окном прокрутки.

use crate::app::input::InputState;
use crate::app::list::{ListCursor, ListNav};
use crate::session::SessionSummary;

/// Служебные сессии цикла GOAL в списке не показываем.
const GOAL_SYSTEM_TAG: &str = "__goal_system__";

#[derive(Debug, Default)]
pub struct ResumeState {
    sessions: Vec<SessionSummary>,
    pub loading: bool,
    /// Все проекты (Tab) или только текущая рабочая папка.
    pub all_projects: bool,
    pub filter: InputState,
    pub cursor: ListCursor,
}

/// Одна ли это папка: без хвостового разделителя и его вида; регистр не важен
/// только на Windows, где `D:\\Work` и `d:\\work` — одно место.
fn same_folder(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let s = s.trim_end_matches(['/', '\\']).replace('\\', "/");
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    norm(a) == norm(b)
}

impl ResumeState {
    /// Открыть экран заново: курсор и фильтр сброшены, список ждёт чтения.
    pub fn open(&mut self) {
        *self = Self {
            loading: true,
            ..Self::default()
        };
    }

    /// Положить прочитанный список.
    pub fn apply(&mut self, sessions: Vec<SessionSummary>) {
        self.sessions = sessions
            .into_iter()
            .filter(|s| s.tag.as_deref() != Some(GOAL_SYSTEM_TAG))
            .collect();
        self.loading = false;
    }

    /// Сессии под текущей выборкой и фильтром, в порядке списка (новые сверху).
    pub fn visible<'a>(&'a self, workspace: &str) -> Vec<&'a SessionSummary> {
        let needle = self.filter.buffer.trim().to_lowercase();
        self.sessions
            .iter()
            .filter(|s| self.in_scope(s, workspace))
            .filter(|s| needle.is_empty() || s.title.to_lowercase().contains(&needle))
            .collect()
    }

    /// Сколько сессий в выборке без фильтра: «9 из 19».
    pub fn scope_len(&self, workspace: &str) -> usize {
        self.sessions
            .iter()
            .filter(|s| self.in_scope(s, workspace))
            .count()
    }

    /// Папка не известна (её не удалось прочитать) — показываем все проекты.
    fn in_scope(&self, session: &SessionSummary, workspace: &str) -> bool {
        self.all_projects || workspace.is_empty() || same_folder(&session.workspace_root, workspace)
    }

    /// Переключить «этот проект / все проекты». Курсор — на начало.
    pub fn toggle_scope(&mut self) {
        self.all_projects = !self.all_projects;
        self.cursor.reset();
    }

    pub fn navigate(&mut self, nav: ListNav, len: usize, page: usize) {
        self.cursor.apply(nav, len, page);
    }

    /// После правки фильтра список сменился: курсор — в его границы.
    pub fn refilter(&mut self, len: usize, page: usize) {
        self.cursor.reset();
        self.cursor.clamp(len, page);
    }

    /// Id сессии под курсором.
    pub fn selected_id(&self, workspace: &str) -> Option<String> {
        self.visible(workspace)
            .get(self.cursor.selected)
            .map(|s| s.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str, folder: &str, title: &str) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            workspace_root: folder.to_string(),
            message_count: 1,
            title: title.to_string(),
            model_type: String::new(),
            tag: None,
            broken: false,
        }
    }

    fn state() -> ResumeState {
        let mut state = ResumeState::default();
        state.open();
        let mut goal = summary("g", "E:\\demo", "goal");
        goal.tag = Some(GOAL_SYSTEM_TAG.to_string());
        state.apply(vec![
            summary("a", "E:\\Demo\\", "Fix range test"),
            summary("b", "E:/demo", "CI workflow"),
            summary("c", "D:\\other", "Memory leak"),
            goal,
        ]);
        state
    }

    /// Разделители и хвостовой слэш не важны нигде, регистр — только на Windows.
    #[test]
    fn the_current_project_is_matched_whatever_the_separators() {
        let state = state();
        let ids = |folder: &str| -> Vec<String> {
            state.visible(folder).iter().map(|s| s.id.clone()).collect()
        };
        if cfg!(windows) {
            assert_eq!(ids("e:\\demo\\"), ["a", "b"]);
        } else {
            assert_eq!(ids("E:\\demo\\"), ["b"]);
            assert_eq!(ids("E:\\Demo"), ["a"]);
        }
    }

    #[test]
    fn an_unknown_folder_shows_every_project() {
        assert_eq!(state().visible("").len(), 3);
    }

    #[test]
    fn tab_widens_to_every_project_but_never_shows_goal_system_sessions() {
        let mut state = state();
        state.toggle_scope();
        let ids: Vec<_> = state
            .visible("e:\\demo")
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn the_filter_matches_titles_without_case() {
        let mut state = state();
        state.toggle_scope();
        state.filter.buffer = "LEAK".to_string();
        assert_eq!(state.selected_id("x").as_deref(), Some("c"));
    }

    #[test]
    fn the_cursor_stays_inside_a_shrinking_list() {
        let mut state = state();
        state.toggle_scope();
        state.navigate(ListNav::Last, 3, 10);
        assert_eq!(state.cursor.selected, 2);
        state.filter.buffer = "ci".to_string();
        let len = state.visible("x").len();
        state.refilter(len, 10);
        assert_eq!(state.selected_id("x").as_deref(), Some("b"));
    }
}
