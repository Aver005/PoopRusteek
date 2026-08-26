//! Report shapes and their rendering.
//!
//! Reports exist to be *compared*: the same scenario run before and after a
//! prompt or parser change should be diffable, so every report is written to
//! disk as JSON and the human rendering is a view of that same data. Rates
//! are reported alongside raw counts because a nondeterministic agent is
//! only ever "mostly" right — a single pass/fail hides the interesting part.

use crate::harness::driver::{RunOutcome, RunStatus};
use crate::harness::metrics::{RunMetrics, TurnEnd};
use crate::harness::scenario::Scenario;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// One repeat: what it did and why it passed or failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub index: usize,
    pub trace_path: PathBuf,
    /// Scratch workspace this repeat ran in, kept for inspection when the
    /// scenario used `workspace_template`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    pub outcome: RunOutcome,
    pub metrics: RunMetrics,
    /// Empty means the run met every expectation.
    pub failures: Vec<String>,
}

impl RunReport {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Min/mean/median/max of one measurement across repeats. Median as well as
/// mean because a single 300s timeout otherwise swamps the average.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stat {
    pub min: f64,
    pub mean: f64,
    pub median: f64,
    pub max: f64,
}

impl Stat {
    fn of(values: &[f64]) -> Self {
        if values.is_empty() {
            return Self::default();
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let middle = sorted.len() / 2;
        let median = if sorted.len().is_multiple_of(2) {
            (sorted[middle - 1] + sorted[middle]) / 2.0
        } else {
            sorted[middle]
        };
        Self {
            min: sorted[0],
            mean: sorted.iter().sum::<f64>() / sorted.len() as f64,
            median,
            max: sorted[sorted.len() - 1],
        }
    }
}

/// Cross-repeat summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Aggregate {
    pub steps: Stat,
    pub tool_calls: Stat,
    pub duration_ms: Stat,
    /// Share of repeats that hit at least one malformed tool call.
    pub malformed_rate: f64,
    /// Share of repeats whose semantic hint was injected.
    pub hint_rate: f64,
    pub turn_ends: BTreeMap<String, usize>,
    /// Total calls per tool across all repeats.
    pub tools: BTreeMap<String, usize>,
}

impl Aggregate {
    fn of(runs: &[RunReport]) -> Self {
        let count = runs.len().max(1) as f64;
        let mut aggregate = Self {
            steps: Stat::of(
                &runs
                    .iter()
                    .map(|r| r.metrics.steps as f64)
                    .collect::<Vec<_>>(),
            ),
            tool_calls: Stat::of(
                &runs
                    .iter()
                    .map(|r| r.metrics.tool_calls as f64)
                    .collect::<Vec<_>>(),
            ),
            duration_ms: Stat::of(
                &runs
                    .iter()
                    .map(|r| r.outcome.duration_ms as f64)
                    .collect::<Vec<_>>(),
            ),
            malformed_rate: runs
                .iter()
                .filter(|r| r.metrics.malformed_tool_calls > 0)
                .count() as f64
                / count,
            hint_rate: runs
                .iter()
                .filter(|r| r.metrics.semantic_hint.is_some())
                .count() as f64
                / count,
            ..Self::default()
        };
        for run in runs {
            *aggregate
                .turn_ends
                .entry(turn_end_name(run.metrics.turn_end).to_string())
                .or_default() += 1;
            for (tool, calls) in &run.metrics.tools_used {
                *aggregate.tools.entry(tool.clone()).or_default() += calls;
            }
        }
        aggregate
    }
}

fn turn_end_name(end: TurnEnd) -> &'static str {
    match end {
        TurnEnd::Done => "done",
        TurnEnd::MaxSteps => "max_steps",
        TurnEnd::Errored => "errored",
        TurnEnd::Unknown => "unknown",
    }
}

/// One scenario's verdict over N repeats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioReport {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub repeats: usize,
    pub passed_runs: usize,
    pub pass_rate: f64,
    pub min_pass_rate: f64,
    pub passed: bool,
    pub aggregate: Aggregate,
    /// Distinct failure reasons and how often each occurred — the fastest
    /// read on *why* a flaky scenario is flaky.
    pub failure_buckets: BTreeMap<String, usize>,
    pub runs: Vec<RunReport>,
}

impl ScenarioReport {
    pub fn new(scenario: &Scenario, runs: Vec<RunReport>, dir: PathBuf) -> Self {
        let repeats = runs.len();
        let passed_runs = runs.iter().filter(|run| run.passed()).count();
        let pass_rate = if repeats == 0 {
            0.0
        } else {
            passed_runs as f64 / repeats as f64
        };
        let min_pass_rate = scenario.expect.min_pass_rate();
        let mut failure_buckets: BTreeMap<String, usize> = BTreeMap::new();
        for run in &runs {
            for failure in &run.failures {
                *failure_buckets.entry(failure.clone()).or_default() += 1;
            }
        }
        Self {
            name: scenario.name.clone(),
            description: scenario.description.clone(),
            dir,
            repeats,
            passed_runs,
            pass_rate,
            min_pass_rate,
            // `>=` with a float threshold is intentional: the common
            // thresholds (1.0, 0.8) are exactly representable at the repeat
            // counts anyone uses.
            passed: pass_rate >= min_pass_rate,
            aggregate: Aggregate::of(&runs),
            failure_buckets,
            runs,
        }
    }
}

/// A whole directory of scenarios.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteReport {
    pub total: usize,
    pub passed_scenarios: usize,
    pub passed: bool,
    pub failed_names: Vec<String>,
    pub scenarios: Vec<ScenarioReport>,
}

impl SuiteReport {
    pub fn new(scenarios: Vec<ScenarioReport>) -> Self {
        let total = scenarios.len();
        let passed_scenarios = scenarios.iter().filter(|report| report.passed).count();
        let failed_names = scenarios
            .iter()
            .filter(|report| !report.passed)
            .map(|report| report.name.clone())
            .collect();
        Self {
            total,
            passed_scenarios,
            passed: passed_scenarios == total,
            failed_names,
            scenarios,
        }
    }
}

/// Human summary of a single `exec`.
pub fn render_run(outcome: &RunOutcome) -> String {
    let mut lines = vec![format!(
        "{} in {}ms",
        status_label(outcome.status),
        outcome.duration_ms
    )];
    if let Some(error) = &outcome.error {
        lines.push(format!("  error:      {error}"));
    }
    if outcome.turns > 1 {
        lines.push(format!("  turns:      {}", outcome.turns));
    }
    if !outcome.tools.is_empty() {
        let names: Vec<String> = outcome
            .tools
            .iter()
            .map(|tool| {
                if tool.approved {
                    tool.name.clone()
                } else {
                    format!("{}(denied)", tool.name)
                }
            })
            .collect();
        lines.push(format!("  tools:      {}", names.join(", ")));
    }
    if outcome.sub_agents > 0 {
        lines.push(format!("  sub-agents: {}", outcome.sub_agents));
    }
    if !outcome.questions.is_empty() {
        lines.push(format!("  questions:  {}", outcome.questions.len()));
    }
    if let Some(session) = &outcome.session_id {
        lines.push(format!("  session:    {session}"));
    }
    lines.push(format!("  trace:      {}", outcome.trace_path.display()));
    if !outcome.final_text.is_empty() {
        lines.push(String::new());
        lines.push(crate::util::truncate_with_ellipsis(
            &outcome.final_text,
            2000,
        ));
    }
    lines.join("\n")
}

fn status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Completed => "✓ completed",
        RunStatus::Failed => "✗ failed",
        RunStatus::TimedOut => "⏱ timed out",
        RunStatus::SetupFailed => "✗ setup failed",
    }
}

pub fn render_scenario(report: &ScenarioReport) -> String {
    let mut lines = vec![
        format!(
            "{} {} — {}/{} passed ({:.0}%, need {:.0}%)",
            if report.passed { "✓" } else { "✗" },
            report.name,
            report.passed_runs,
            report.repeats,
            report.pass_rate * 100.0,
            report.min_pass_rate * 100.0
        ),
        format!(
            "  steps      min {:.0} / med {:.0} / max {:.0}",
            report.aggregate.steps.min, report.aggregate.steps.median, report.aggregate.steps.max
        ),
        format!(
            "  tool calls min {:.0} / med {:.0} / max {:.0}",
            report.aggregate.tool_calls.min,
            report.aggregate.tool_calls.median,
            report.aggregate.tool_calls.max
        ),
        format!(
            "  duration   med {:.1}s / max {:.1}s",
            report.aggregate.duration_ms.median / 1000.0,
            report.aggregate.duration_ms.max / 1000.0
        ),
        format!(
            "  malformed  {:.0}% of runs      hint injected {:.0}%",
            report.aggregate.malformed_rate * 100.0,
            report.aggregate.hint_rate * 100.0
        ),
    ];
    // Only shown for a conversation: a one-turn run is the norm and saying so
    // on every line would be noise.
    let turns: Vec<usize> = report.runs.iter().map(|run| run.outcome.turns).collect();
    if let (Some(&low), Some(&high)) = (turns.iter().min(), turns.iter().max())
        && high > 1
    {
        let range = if low == high {
            high.to_string()
        } else {
            format!("{low}–{high}")
        };
        lines.push(format!("  turns      {range} per run"));
    }
    if !report.aggregate.tools.is_empty() {
        let tools: Vec<String> = report
            .aggregate
            .tools
            .iter()
            .map(|(name, calls)| format!("{name}×{calls}"))
            .collect();
        lines.push(format!("  tools      {}", tools.join(", ")));
    }
    let ends: Vec<String> = report
        .aggregate
        .turn_ends
        .iter()
        .map(|(end, count)| format!("{end}×{count}"))
        .collect();
    lines.push(format!("  turn end   {}", ends.join(", ")));
    if !report.failure_buckets.is_empty() {
        lines.push("  failures:".to_string());
        let mut buckets: Vec<(&String, &usize)> = report.failure_buckets.iter().collect();
        buckets.sort_by(|a, b| b.1.cmp(a.1));
        for (reason, count) in buckets {
            lines.push(format!("    {count}× {reason}"));
        }
    }
    lines.join("\n")
}

pub fn render_suite(report: &SuiteReport) -> String {
    let mut lines: Vec<String> = report.scenarios.iter().map(render_scenario).collect();
    lines.push(String::new());
    lines.push(format!(
        "{} {}/{} scenarios passed",
        if report.passed { "✓" } else { "✗" },
        report.passed_scenarios,
        report.total
    ));
    if !report.failed_names.is_empty() {
        lines.push(format!("  failed: {}", report.failed_names.join(", ")));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_reports_median_not_just_mean() {
        // One outlier: the mean moves, the median does not.
        let stat = Stat::of(&[1.0, 1.0, 1.0, 100.0]);
        assert_eq!(stat.min, 1.0);
        assert_eq!(stat.max, 100.0);
        assert_eq!(stat.median, 1.0);
        assert!((stat.mean - 25.75).abs() < f64::EPSILON);
    }

    #[test]
    fn stat_of_empty_is_zeroed_not_nan() {
        let stat = Stat::of(&[]);
        assert_eq!(stat.mean, 0.0);
        assert!(!stat.median.is_nan());
    }

    #[test]
    fn odd_length_median_picks_the_middle() {
        assert_eq!(Stat::of(&[3.0, 1.0, 2.0]).median, 2.0);
    }

    #[test]
    fn aggregate_counts_rates_and_tool_totals() {
        let runs = vec![run(2, 1, 0, Some("skill:a")), run(4, 3, 1, None)];
        let aggregate = Aggregate::of(&runs);
        assert_eq!(aggregate.steps.median, 3.0);
        assert!((aggregate.malformed_rate - 0.5).abs() < f64::EPSILON);
        assert!((aggregate.hint_rate - 0.5).abs() < f64::EPSILON);
        assert_eq!(aggregate.tools.get("bash"), Some(&4));
        assert_eq!(aggregate.turn_ends.get("done"), Some(&2));
    }

    #[test]
    fn suite_fails_when_any_scenario_fails() {
        let passing = report_with(1.0, 1.0);
        let failing = report_with(0.5, 1.0);
        assert!(SuiteReport::new(vec![passing.clone()]).passed);
        let mixed = SuiteReport::new(vec![passing, failing]);
        assert!(!mixed.passed);
        assert_eq!(mixed.passed_scenarios, 1);
        assert_eq!(mixed.failed_names, vec!["s".to_string()]);
    }

    fn run(steps: usize, tool_calls: usize, malformed: usize, hint: Option<&str>) -> RunReport {
        let mut metrics = RunMetrics {
            steps,
            tool_calls,
            malformed_tool_calls: malformed,
            semantic_hint: hint.map(str::to_string),
            turn_end: TurnEnd::Done,
            ..RunMetrics::default()
        };
        metrics.tools_used.insert("bash".to_string(), tool_calls);
        RunReport {
            index: 0,
            trace_path: PathBuf::from("t.jsonl"),
            workspace: None,
            outcome: RunOutcome {
                status: RunStatus::Completed,
                trace_path: PathBuf::from("t.jsonl"),
                duration_ms: 1000,
                turns: 1,
                final_text: String::new(),
                tools: Vec::new(),
                sub_agents: 0,
                questions: Vec::new(),
                error: None,
                session_id: None,
                semantic_ready: false,
            },
            metrics,
            failures: Vec::new(),
        }
    }

    fn report_with(pass_rate: f64, min_pass_rate: f64) -> ScenarioReport {
        ScenarioReport {
            name: "s".into(),
            description: String::new(),
            dir: PathBuf::from("."),
            repeats: 2,
            passed_runs: 1,
            pass_rate,
            min_pass_rate,
            passed: pass_rate >= min_pass_rate,
            aggregate: Aggregate::default(),
            failure_buckets: BTreeMap::new(),
            runs: Vec::new(),
        }
    }
}
