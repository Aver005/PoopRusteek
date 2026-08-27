//! `/search` — the history-search screen (`View::Search`).
//!
//! State + pure presentation logic for searching the semantic history
//! index: a query input, fetched matches (relevance-ordered, as returned
//! by the store), and client-side sort/filter/dedup applied on top via
//! [`visible_indices`] — pure and unit-tested. The actual lookup runs off
//! the event loop (`App::spawn_history_search` → blocking thread →
//! `AppEvent::HistorySearchDone`).

use crate::app::App;
use crate::app::events::AppEvent;
use crate::app::input::InputState;
use crate::semantic::history::HistoryMatch;
use std::sync::Arc;

/// Which zone owns plain keystrokes: the query line (text editing) or the
/// result list (navigation + single-letter actions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchFocus {
    #[default]
    Query,
    Results,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchSort {
    /// Order returned by the hybrid ranker.
    #[default]
    Relevance,
    Newest,
    Oldest,
}

impl SearchSort {
    pub fn next(self) -> Self {
        match self {
            Self::Relevance => Self::Newest,
            Self::Newest => Self::Oldest,
            Self::Oldest => Self::Relevance,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Relevance => "relevance",
            Self::Newest => "newest",
            Self::Oldest => "oldest",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoleFilter {
    #[default]
    All,
    User,
    Assistant,
}

impl RoleFilter {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::User,
            Self::User => Self::Assistant,
            Self::Assistant => Self::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }

    fn admits(self, role: &str) -> bool {
        match self {
            Self::All => true,
            Self::User => role == "user",
            Self::Assistant => role == "assistant",
        }
    }
}

#[derive(Default)]
pub struct SearchViewState {
    pub query: InputState,
    pub focus: SearchFocus,
    pub sort: SearchSort,
    pub role_filter: RoleFilter,
    /// Show only the best (most relevant) chunk of each session.
    pub unique_sessions: bool,
    /// Matches in the ranker's relevance order — never reordered in place;
    /// sort/filter produce an index view over this.
    pub matches: Vec<HistoryMatch>,
    /// Selection is a position in the *visible* (filtered+sorted) list,
    /// not in `matches`; the render derives its scroll window from it.
    pub selected: usize,
    pub searching: bool,
    /// The query the current `matches` answer — a late reply for anything
    /// else is stale and dropped.
    pub last_query: String,
    pub status: String,
}

impl SearchViewState {
    /// Fresh state for opening the view, optionally with a prefilled query.
    pub fn fresh(query: &str) -> Self {
        let mut state = Self::default();
        state.query.buffer = query.to_string();
        state.query.cursor = query.chars().count();
        state
    }

    /// Indices into `matches`, filtered and sorted for display.
    pub fn visible(&self) -> Vec<usize> {
        visible_indices(
            &self.matches,
            self.sort,
            self.role_filter,
            self.unique_sessions,
        )
    }

    /// Поздний ответ на прошлый запрос отбрасывается — иначе он затёр бы
    /// то, что человек спрашивает сейчас.
    pub fn apply_results(&mut self, query: &str, matches: Vec<HistoryMatch>) {
        if self.last_query != query {
            return;
        }
        self.searching = false;
        self.matches = matches;
        self.reset_selection();
        self.status = if self.matches.is_empty() {
            "No matches — the index may still be building (see /rag)".to_string()
        } else {
            format!("{} matches", self.matches.len())
        };
        // Сразу поставить курсор на результаты, а не на строку запроса.
        if !self.matches.is_empty() {
            self.focus = SearchFocus::Results;
        }
    }

    /// Reset list position after anything that changes the visible set.
    pub fn reset_selection(&mut self) {
        self.selected = 0;
    }
}

/// Pure view computation: role filter → per-session dedup (keeps the
/// most relevant chunk, which is the first in base order) → sort.
pub fn visible_indices(
    matches: &[HistoryMatch],
    sort: SearchSort,
    role_filter: RoleFilter,
    unique_sessions: bool,
) -> Vec<usize> {
    let mut seen_sessions = std::collections::HashSet::new();
    let mut indices: Vec<usize> = (0..matches.len())
        .filter(|&i| role_filter.admits(&matches[i].role))
        .filter(|&i| !unique_sessions || seen_sessions.insert(matches[i].session_id.clone()))
        .collect();

    match sort {
        SearchSort::Relevance => {}
        // RFC 3339 timestamps compare correctly as strings; empty stamps
        // sort last under Newest (they compare below every real date).
        SearchSort::Newest => {
            indices.sort_by(|&a, &b| matches[b].timestamp.cmp(&matches[a].timestamp))
        }
        SearchSort::Oldest => {
            indices.sort_by(|&a, &b| matches[a].timestamp.cmp(&matches[b].timestamp))
        }
    }
    indices
}

impl App {
    /// Kick off a history lookup for the search view. The blocking-thread
    /// result comes back as `HistorySearchDone` and is dropped if the user
    /// has already searched for something else.
    pub(crate) fn spawn_history_search(&mut self, query: String) {
        self.state.search.searching = true;
        self.state.search.last_query = query.clone();
        self.state.search.status = String::new();
        let semantic = Arc::clone(&self.semantic);
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            let for_search = query.clone();
            // Over-fetch: filters/dedup are applied client-side, so give
            // them enough raw material to stay useful.
            let matches =
                tokio::task::spawn_blocking(move || semantic.search_history(&for_search, 50))
                    .await
                    .unwrap_or_default();
            let _ = event_tx.send(AppEvent::HistorySearchDone { query, matches });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(session: &str, role: &str, ts: &str) -> HistoryMatch {
        HistoryMatch {
            session_id: session.to_string(),
            title: format!("title-{session}"),
            role: role.to_string(),
            timestamp: ts.to_string(),
            text: String::new(),
        }
    }

    fn fixture() -> Vec<HistoryMatch> {
        vec![
            hit("s1", "assistant", "2026-07-03T10:00:00Z"), // 0: most relevant
            hit("s2", "user", "2026-07-05T09:00:00Z"),      // 1
            hit("s1", "user", "2026-07-01T08:00:00Z"),      // 2
            hit("s3", "assistant", ""),                     // 3: no timestamp
        ]
    }

    #[test]
    fn relevance_keeps_base_order() {
        let v = visible_indices(&fixture(), SearchSort::Relevance, RoleFilter::All, false);
        assert_eq!(v, vec![0, 1, 2, 3]);
    }

    #[test]
    fn role_filter_narrows() {
        let v = visible_indices(&fixture(), SearchSort::Relevance, RoleFilter::User, false);
        assert_eq!(v, vec![1, 2]);
    }

    #[test]
    fn unique_sessions_keeps_most_relevant_chunk() {
        let v = visible_indices(&fixture(), SearchSort::Relevance, RoleFilter::All, true);
        assert_eq!(
            v,
            vec![0, 1, 3],
            "s1's second (less relevant) chunk must drop"
        );
    }

    #[test]
    fn newest_sorts_by_timestamp_desc_with_empty_last() {
        let v = visible_indices(&fixture(), SearchSort::Newest, RoleFilter::All, false);
        assert_eq!(v, vec![1, 0, 2, 3]);
    }

    #[test]
    fn oldest_sorts_by_timestamp_asc() {
        let v = visible_indices(&fixture(), SearchSort::Oldest, RoleFilter::All, false);
        assert_eq!(v, vec![3, 2, 0, 1]);
    }

    #[test]
    fn fresh_prefills_query_with_cursor_at_end() {
        let s = SearchViewState::fresh("тест");
        assert_eq!(s.query.buffer, "тест");
        assert_eq!(s.query.cursor, 4);
    }
}
