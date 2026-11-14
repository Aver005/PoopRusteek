//! Панель «идёт работа» над полем ввода: что делается сейчас, суб-агенты,
//! очередь сообщений, GOAL, фон и ошибки. Появляется, пока что-то работает,
//! и исчезает в простое. Высота — не больше [`MAX_DOCK_ROWS`] строк.

use super::segments::{Fitted, Segment, Tone, fit_segments};
use super::util::split_line;
use crate::app::AppState;
use crate::app::conversation::{Conversation, ConversationKind};
use crate::app::events::GoalStage;
use crate::tui::glyphs;
use crate::tui::theme::Theme;
use crate::tui::view_model;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Меньше всего строк панели: на низком окне остальное уходит в «+N».
pub(crate) const MAX_DOCK_ROWS: u16 = 4;

/// Потолок панели зависит от высоты окна: на высоком экране параллельная работа видна целиком.
pub(crate) fn max_rows(window_rows: u16) -> u16 {
    match window_rows {
        0..30 => MAX_DOCK_ROWS,
        30..40 => 6,
        _ => 8,
    }
}
/// Сколько символов сообщения из очереди показываем.
const QUEUE_PREVIEW_CHARS: usize = 60;

/// Что делает фокусный чат прямо сейчас.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MainRow {
    pub tick: u64,
    pub tool: Option<(String, String)>,
    pub elapsed_secs: u64,
    pub tokens: u32,
    pub tokens_per_sec: u32,
}

/// Суб-агент или побочный чат в работе.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ChildRow {
    pub label: String,
    pub tool: Option<(String, String)>,
    pub elapsed_secs: u64,
    /// Сколько уже идёт его инструмент (если он есть).
    pub tool_secs: Option<u64>,
}

/// Содержимое панели без привязки к ширине.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DockModel {
    pub main: Option<MainRow>,
    pub children: Vec<ChildRow>,
    /// Сколько сообщений ждёт, что первое, и стоит ли очередь после упавшего хода.
    pub queue: Option<(usize, String, bool)>,
    /// Сколько агентов не поместилось. Показаны отдельной строкой дерева;
    /// ни с очередью, ни со сводкой GOAL они не смешиваются.
    pub hidden_agents: usize,
    summary: Vec<SummaryItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SummaryItem {
    priority: u8,
    tone: Tone,
    long: String,
    short: String,
}

impl DockModel {
    /// Отдельная строка «+N агентов»: когда скрытые есть.
    fn overflow_row(&self) -> bool {
        self.hidden_agents > 0
    }

    fn rows(&self) -> u16 {
        let rows = usize::from(self.main.is_some())
            + self.children.len()
            + usize::from(self.overflow_row())
            + usize::from(self.queue.is_some())
            + usize::from(!self.summary.is_empty());
        rows as u16
    }
}

fn is_child(conversation: &Conversation) -> bool {
    matches!(
        conversation.kind,
        ConversationKind::Sidechat | ConversationKind::SubAgent
    )
}

fn tool_of(conversation: &Conversation) -> Option<(String, String)> {
    conversation
        .current_tool
        .as_ref()
        .map(|run| (run.name.clone(), run.summary.clone()))
}

fn elapsed_secs(conversation: &Conversation) -> u64 {
    conversation
        .generation
        .start_time
        .map_or(0, |start| start.elapsed().as_secs())
}

fn goal_text(state: &AppState) -> Option<(String, String)> {
    if !state.goal.mode {
        return None;
    }
    let (long, short) = match state.goal.stage {
        GoalStage::Inactive => ("GOAL on".to_string(), "GOAL".to_string()),
        GoalStage::WaitForGoal => ("GOAL waiting for a goal".to_string(), "GOAL".to_string()),
        GoalStage::RunAgent1 => (
            format!("GOAL iter {} · build", state.goal.iteration),
            format!("GOAL {}", state.goal.iteration),
        ),
        GoalStage::RunEvaluator => (
            format!("GOAL iter {} · eval", state.goal.iteration),
            format!("GOAL {}", state.goal.iteration),
        ),
        GoalStage::Done => ("GOAL done".to_string(), "GOAL".to_string()),
    };
    Some((long, short))
}

/// Идёт ли работа, ради которой стоит показывать панель. Очередь, что стоит
/// после упавшего хода, тоже требует внимания: без панели о ней не узнать.
fn work_is_active(state: &AppState, children_running: usize) -> bool {
    let focused = state.focused();
    focused.generation.active
        || children_running > 0
        || state.goal.is_running()
        || (focused.queue_paused && !focused.queued.is_empty())
}

pub(super) fn model(state: &AppState) -> DockModel {
    let focused_id = state.conversations.focused_id();
    let running_children: Vec<&Conversation> = state
        .conversations
        .iter()
        .filter(|conv| is_child(conv) && conv.is_streaming() && conv.id != focused_id)
        .collect();
    if !work_is_active(state, running_children.len()) {
        return DockModel::default();
    }

    let focused = state.focused();
    let main = focused.generation.active.then(|| {
        let elapsed = focused
            .generation
            .start_time
            .map_or(0.0, |start| start.elapsed().as_secs_f64());
        MainRow {
            tick: focused.generation.animation_tick,
            tool: tool_of(focused),
            elapsed_secs: elapsed as u64,
            tokens: focused.generation.last_tokens,
            tokens_per_sec: view_model::tokens_per_sec(focused.generation.last_tokens, elapsed)
                as u32,
        }
    });
    let queue = (!focused.queued.is_empty()).then(|| {
        let next = focused
            .queued
            .front()
            .map(|message| crate::util::one_line_label(&message.text, QUEUE_PREVIEW_CHARS))
            .unwrap_or_default();
        (focused.queued.len(), next, focused.queue_paused)
    });

    let mut summary = Vec::new();
    if let Some((long, short)) = goal_text(state) {
        summary.push(SummaryItem {
            priority: 1,
            tone: Tone::Accent,
            long,
            short,
        });
    }
    if state.background.total > 0 {
        let text = format!("bg {}", state.background.total);
        summary.push(SummaryItem {
            priority: 3,
            tone: Tone::Dim,
            long: text.clone(),
            short: text,
        });
    }
    if state.timers_pending > 0 {
        let text = format!("timers {}", state.timers_pending);
        summary.push(SummaryItem {
            priority: 4,
            tone: Tone::Dim,
            long: text.clone(),
            short: text,
        });
    }
    if state.error_count > 0 {
        let glyph = glyphs::get().warn;
        let noun = if state.error_count == 1 {
            "error"
        } else {
            "errors"
        };
        summary.push(SummaryItem {
            priority: 2,
            tone: Tone::Error,
            long: format!("{glyph} {} {noun}", state.error_count),
            short: format!("{glyph} {}", state.error_count),
        });
    }

    let main_rows = usize::from(main.is_some());
    let queue_rows = usize::from(queue.is_some());
    let max_rows = usize::from(max_rows(state.terminal_rows));
    let mut budget =
        max_rows.saturating_sub(main_rows + queue_rows + usize::from(!summary.is_empty()));
    // Скрытые агенты получают свою строку — её место отнимается у списка.
    if running_children.len() > budget {
        budget = budget.saturating_sub(1);
    }
    let shown = running_children.len().min(budget);
    let hidden_agents = running_children.len() - shown;

    let children = running_children
        .iter()
        .take(shown)
        .map(|conv| ChildRow {
            label: crate::util::one_line_label(&conv.title, 40),
            tool: tool_of(conv),
            elapsed_secs: elapsed_secs(conv),
            tool_secs: conv
                .current_tool
                .as_ref()
                .map(|run| run.started.elapsed().as_secs()),
        })
        .collect();

    DockModel {
        main,
        children,
        queue,
        hidden_agents,
        summary,
    }
}

/// Сколько строк займёт панель в этом состоянии (0 в простое).
pub(crate) fn height(state: &AppState) -> u16 {
    model(state).rows().min(max_rows(state.terminal_rows))
}

/// `m:ss`.
pub(super) fn clock(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// «1.8k» для больших чисел.
pub(super) fn compact_count(value: u32) -> String {
    if value >= 1000 {
        format!("{}.{}k", value / 1000, (value % 1000) / 100)
    } else {
        value.to_string()
    }
}

pub(super) fn tool_text(tool: &Option<(String, String)>) -> (String, String) {
    match tool {
        Some((name, summary)) => (name.clone(), summary.clone()),
        None => ("thinking".to_string(), String::new()),
    }
}

/// Строка «слева — суть, справа — время», усечённая под ширину.
fn two_sided<'a>(width: usize, lead: Vec<Span<'a>>, right: &str, theme: &Theme) -> Line<'a> {
    split_line(
        lead,
        vec![Span::styled(
            right.to_string(),
            Style::default().fg(theme.text_dim),
        )],
        width,
    )
}

fn main_line<'a>(row: &MainRow, width: usize, theme: &Theme) -> Line<'a> {
    let spin = glyphs::get().spin(row.tick);
    let (name, summary) = tool_text(&row.tool);
    let mut lead = vec![
        Span::styled(
            format!(" {spin} "),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            name,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !summary.is_empty() {
        lead.push(Span::styled(
            format!(" · {summary}"),
            Style::default().fg(theme.text_soft),
        ));
    }
    let mut right = clock(row.elapsed_secs);
    if row.tokens > 0 {
        right.push_str(&format!(" · {} tok", compact_count(row.tokens)));
    }
    if row.tokens_per_sec > 0 {
        right.push_str(&format!(" · {} t/s", row.tokens_per_sec));
    }
    right.push(' ');
    two_sided(width, lead, &right, theme)
}

fn child_line<'a>(row: &ChildRow, last: bool, width: usize, theme: &Theme) -> Line<'a> {
    let branch = if last { "\u{2514}" } else { "\u{251C}" };
    let (name, summary) = tool_text(&row.tool);
    let detail = if summary.is_empty() {
        name
    } else {
        format!("{name} {summary}")
    };
    let lead = vec![
        Span::styled(format!(" {branch} "), Style::default().fg(theme.text_dim)),
        Span::styled("agent ", Style::default().fg(theme.text_dim)),
        Span::styled(row.label.clone(), Style::default().fg(theme.fg)),
        Span::styled(format!(" · {detail}"), Style::default().fg(theme.text_soft)),
    ];
    // У агента с инструментом показываем, как давно идёт именно он.
    let right = format!("{} ", clock(row.tool_secs.unwrap_or(row.elapsed_secs)));
    two_sided(width, lead, &right, theme)
}

fn queue_line<'a>(count: usize, next: &str, paused: bool, width: usize, theme: &Theme) -> Line<'a> {
    if paused {
        // Ход упал, а сообщения ждут решения человека.
        let text = format!(
            " turn failed \u{00B7} {count} queued \u{00B7} Enter sends the next \u{00B7} Esc returns them to the input"
        );
        let lead = vec![Span::styled(
            text,
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
        )];
        return two_sided(width, lead, "", theme);
    }
    let lead = vec![
        Span::styled(
            format!(" queued {count}"),
            Style::default().fg(theme.warning),
        ),
        Span::styled(
            format!(" \u{00B7} next: {next}"),
            Style::default().fg(theme.text_soft),
        ),
    ];
    two_sided(width, lead, "", theme)
}

fn tone_style(tone: Tone, theme: &Theme) -> Style {
    match tone {
        Tone::Plain => Style::default().fg(theme.fg),
        Tone::Dim => Style::default().fg(theme.text_dim),
        Tone::Accent => Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
        Tone::Warning => Style::default().fg(theme.warning),
        Tone::Error => Style::default()
            .fg(theme.error)
            .add_modifier(Modifier::BOLD),
    }
}

fn summary_line<'a>(items: &[SummaryItem], width: usize, theme: &Theme) -> Line<'a> {
    let segments = items
        .iter()
        .map(|item| {
            Segment::new(item.priority, item.tone, item.long.clone()).or_short(item.short.clone())
        })
        .collect();
    // Левая группа лежит одной строкой, правой нет.
    let (left, _): (Vec<Fitted>, Vec<Fitted>) =
        fit_segments(width.saturating_sub(1), segments, Vec::new());
    let mut spans = vec![Span::raw(" ")];
    for (index, part) in left.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", Style::default().fg(theme.text_dim)));
        }
        spans.push(Span::styled(part.text, tone_style(part.tone, theme)));
    }
    Line::from(spans)
}

pub(crate) fn render_dock(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    let model = model(state);
    let width = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    if let Some(main) = &model.main {
        lines.push(main_line(main, width, theme));
    }
    let child_count = model.children.len();
    let overflow = model.overflow_row();
    for (index, child) in model.children.iter().enumerate() {
        let last = index + 1 == child_count && !overflow;
        lines.push(child_line(child, last, width, theme));
    }
    if overflow {
        let branch = "\u{2514}";
        lines.push(Line::from(vec![
            Span::styled(format!(" {branch} "), Style::default().fg(theme.text_dim)),
            Span::styled(
                format!("+{} more agents", model.hidden_agents),
                Style::default().fg(theme.text_dim),
            ),
        ]));
    }
    if let Some((count, next, paused)) = &model.queue {
        lines.push(queue_line(*count, next, *paused, width, theme));
    }
    if !model.summary.is_empty() {
        lines.push(summary_line(&model.summary, width, theme));
    }
    lines.truncate(area.height as usize);
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::conversation::Conversation;

    fn idle_state() -> AppState {
        AppState::new(Conversation::fresh_main(None))
    }

    fn start_agent(state: &mut AppState, title: &str) {
        let mut agent = Conversation::fresh_main(None);
        agent.kind = ConversationKind::SubAgent;
        agent.parent = Some(state.conversations.focused_id());
        agent.title = title.to_string();
        agent.generation.active = true;
        state.conversations.add_background(agent);
    }

    #[test]
    fn the_dock_is_empty_when_nothing_is_running() {
        assert_eq!(height(&idle_state()), 0);
    }

    #[test]
    fn persistent_background_jobs_alone_do_not_open_the_dock() {
        let mut state = idle_state();
        state.background.total = 2;
        state.timers_pending = 1;
        assert_eq!(height(&state), 0);
    }

    #[test]
    fn a_running_turn_shows_one_line() {
        let mut state = idle_state();
        state.focused_mut().generation.active = true;
        assert_eq!(height(&state), 1);
    }

    #[test]
    fn agents_goal_and_background_fill_the_panel_but_not_more_than_four_rows() {
        let mut state = idle_state();
        state.focused_mut().generation.active = true;
        state.goal.mode = true;
        state.goal.stage = GoalStage::RunAgent1;
        state.background.total = 2;
        for index in 0..6 {
            start_agent(&mut state, &format!("agent {index}"));
        }
        assert_eq!(height(&state), MAX_DOCK_ROWS);
        let model = model(&state);
        assert!(model.children.len() < 6);
        assert!(model.hidden_agents > 0 && model.overflow_row());
        assert!(
            model.summary.iter().all(|item| !item.long.starts_with('+')),
            "скрытые агенты не смешиваются со сводкой GOAL"
        );
    }

    #[test]
    fn only_agents_without_a_main_turn_still_open_the_dock() {
        let mut state = idle_state();
        start_agent(&mut state, "docs");
        let model = model(&state);
        assert!(model.main.is_none());
        assert_eq!(model.children.len(), 1);
        assert_eq!(height(&state), 1);
    }

    #[test]
    fn queued_messages_are_shown_with_the_next_one() {
        let mut state = idle_state();
        state.focused_mut().generation.active = true;
        state
            .focused_mut()
            .queued
            .push_back(crate::app::conversation::QueuedMessage {
                text: "add a test for the empty string".to_string(),
                chips: Vec::new(),
                named: Vec::new(),
            });
        let model = model(&state);
        assert_eq!(model.queue.as_ref().map(|(count, _, _)| *count), Some(1));
        assert_eq!(height(&state), 2);
    }

    #[test]
    fn clocks_and_counts_are_compact() {
        assert_eq!(clock(47), "0:47");
        assert_eq!(clock(125), "2:05");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1840), "1.8k");
    }

    #[test]
    fn a_queue_held_after_a_failed_turn_keeps_the_dock_open() {
        let mut state = idle_state();
        state
            .focused_mut()
            .queued
            .push_back(crate::app::conversation::QueuedMessage {
                text: "next".to_string(),
                chips: Vec::new(),
                named: Vec::new(),
            });
        assert_eq!(height(&state), 0, "очередь без остановки уйдёт сама");
        state.focused_mut().queue_paused = true;
        assert_eq!(height(&state), 1);
        let model = model(&state);
        assert_eq!(model.queue.map(|(_, _, paused)| paused), Some(true));
    }

    /// Есть очередь — «+N агентов» уходит в её строку, отдельной строки нет.
    #[test]
    fn hidden_agents_get_their_own_row_even_when_there_is_a_queue() {
        let mut state = idle_state();
        state.focused_mut().generation.active = true;
        state
            .focused_mut()
            .queued
            .push_back(crate::app::conversation::QueuedMessage {
                text: "next".to_string(),
                chips: Vec::new(),
                named: Vec::new(),
            });
        for index in 0..6 {
            start_agent(&mut state, &format!("agent {index}"));
        }
        let model = model(&state);
        assert!(model.hidden_agents > 0);
        assert!(
            model.overflow_row(),
            "«+N more agents» is not part of the queue row"
        );
        assert!(model.rows() <= MAX_DOCK_ROWS);
    }

    #[test]
    fn a_tall_window_shows_more_parallel_work() {
        let mut state = idle_state();
        state.focused_mut().generation.active = true;
        for index in 0..6 {
            start_agent(&mut state, &format!("agent {index}"));
        }
        state.terminal_rows = 24;
        let low = model(&state);
        state.terminal_rows = 50;
        let tall = model(&state);
        assert!(tall.children.len() > low.children.len());
        assert_eq!(tall.hidden_agents, 0, "six agents fit in a tall window");
        assert!(tall.rows() <= max_rows(50));
    }
}
