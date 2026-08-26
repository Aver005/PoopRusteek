//! Scenario files: one or more user turns, the conditions to run them under,
//! and what the result must look like — repeated, because one sample of a
//! nondeterministic model is not evidence.
//!
//! `prompt` is one turn; `prompts` is a conversation, run in order against one
//! accumulating history. Expectations always describe the *final* state: the
//! last turn's answer and the workspace once every turn is done.
//!
//! Each repeat runs as a **child process** (`pooprusteek exec --json`)
//! rather than an in-process loop. That buys three things worth the spawn
//! cost: a fresh trace file per repeat (the debug log is a process-global
//! sink), no state bleeding between repeats, and a hung or panicking turn
//! that can be killed without taking the runner down with it.

use crate::error::{AppError, AppResult};
use crate::harness::driver::{RunOutcome, RunStatus};
use crate::harness::metrics::RunMetrics;
use crate::harness::report::{self, RunReport, ScenarioReport, SuiteReport};
use crate::harness::trace::Trace;
use crate::harness::{ScenarioArgs, SuiteArgs, run_stamp};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Exit code for "ran fine, but the expectations were not met".
pub const EXIT_EXPECTATIONS_FAILED: i32 = 4;

/// One scenario, as parsed from TOML. `deny_unknown_fields` is deliberate:
/// a mistyped expectation that silently passes is the worst failure a test
/// harness can have.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// One user turn. Mutually exclusive with `prompts`; folded into it at
    /// load time, so [`Scenario::turns`] is the only reader either way.
    prompt: Option<String>,
    /// Several user turns, run in order in **one** conversation against one
    /// accumulating history — what makes anything between turns testable.
    #[serde(default)]
    prompts: Vec<String>,
    /// Workspace for the turn, resolved relative to the scenario file. Shared
    /// by every repeat and treated as read-only — use `workspace_template` for
    /// anything the agent is meant to write to.
    pub workspace: Option<PathBuf>,
    /// Directory copied into a **fresh scratch workspace per repeat**, so a
    /// task that creates or edits files starts from a known state every time
    /// and cannot see what a previous repeat left behind. An empty template
    /// directory is fine — that is a greenfield task. The copies are kept
    /// under the report directory so the agent's actual output can be
    /// inspected after the fact. Resolved relative to the scenario file.
    pub workspace_template: Option<PathBuf>,
    /// File appended to the system prompt for this scenario, resolved
    /// relative to the scenario file. This is how one task is run under
    /// several prompt variants and the results compared.
    pub system_prompt_append: Option<PathBuf>,
    #[serde(default = "default_approve")]
    pub approve: String,
    pub answer: Option<String>,
    /// Step cap for *each* turn, unlike `expect.max_steps` which is the total.
    pub max_steps: Option<usize>,
    /// Wall-clock budget for the whole run in seconds — every turn together,
    /// not one budget per turn.
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default = "default_semantic")]
    pub semantic: String,
    #[serde(default)]
    pub mcp: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub save_session: bool,
    /// Default repeat count; `--repeat` on the command line wins.
    pub repeat: Option<usize>,
    #[serde(default)]
    pub expect: Expect,
}

fn default_approve() -> String {
    "all".to_string()
}

fn default_timeout() -> u64 {
    300
}

fn default_semantic() -> String {
    "off".to_string()
}

/// What a run must look like to count as a pass. Always the *final* state:
/// the last turn's answer and the workspace after every turn has run.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// `completed` | `failed` | `timed_out` | `setup_failed`.
    pub status: Option<String>,
    /// Every one of these tools must have been called.
    #[serde(default)]
    pub tools_used: Vec<String>,
    /// None of these may have been called.
    #[serde(default)]
    pub tools_forbidden: Vec<String>,
    /// Upper bound on agent steps — catches a turn that wanders. Counted
    /// across every turn, not per turn.
    pub max_steps: Option<usize>,
    pub max_tool_calls: Option<usize>,
    /// No malformed `<tool_use>` blocks at all.
    #[serde(default)]
    pub no_malformed: bool,
    /// Regexes the final answer must match.
    #[serde(default)]
    pub final_matches: Vec<String>,
    /// Regexes the final answer must not match.
    #[serde(default)]
    pub final_not_matches: Vec<String>,
    /// Substring the injected semantic hint must contain — the way to assert
    /// RAG picked the right skill or tool.
    pub semantic_hint_contains: Option<String>,
    /// Files that must exist in the workspace afterwards, relative to it.
    /// For a real development task this is the only expectation that matters:
    /// the answer text can look perfect while nothing was actually written.
    #[serde(default)]
    pub files_exist: Vec<String>,
    /// Files that must *not* exist — catches an agent that scatters scratch
    /// files, or writes to the path it was told to leave alone.
    #[serde(default)]
    pub files_absent: Vec<String>,
    /// Regexes that must match the contents of a file in the workspace.
    #[serde(default)]
    pub file_matches: Vec<FileExpect>,
    /// Fraction of repeats that must pass. Defaults to 1.0.
    pub min_pass_rate: Option<f64>,
}

/// One "this file must contain this" assertion.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileExpect {
    /// Path relative to the workspace.
    pub path: String,
    /// Regex the file's contents must match. Use a TOML literal string.
    pub pattern: String,
    /// Invert the check: the pattern must *not* appear.
    #[serde(default)]
    pub absent: bool,
}

impl Expect {
    pub fn min_pass_rate(&self) -> f64 {
        self.min_pass_rate.unwrap_or(1.0)
    }

    /// Reasons this run failed. Empty means it passed. `workspace` is where
    /// the file expectations are resolved; `None` skips them (nothing was
    /// writable, so there is nothing to inspect).
    pub fn check(
        &self,
        outcome: &RunOutcome,
        metrics: &RunMetrics,
        workspace: Option<&Path>,
    ) -> AppResult<Vec<String>> {
        let mut failures = Vec::new();

        if let Some(expected) = &self.status {
            let actual = status_name(outcome.status);
            if actual != expected.as_str() {
                failures.push(format!("status: expected {expected}, got {actual}"));
            }
        }

        for tool in &self.tools_used {
            if !metrics.tools_used.contains_key(tool) {
                failures.push(format!("tool '{tool}' was never called"));
            }
        }
        for tool in &self.tools_forbidden {
            if metrics.tools_used.contains_key(tool) {
                failures.push(format!("forbidden tool '{tool}' was called"));
            }
        }

        if let Some(limit) = self.max_steps
            && metrics.steps > limit
        {
            failures.push(format!("took {} steps, limit {limit}", metrics.steps));
        }
        if let Some(limit) = self.max_tool_calls
            && metrics.tool_calls > limit
        {
            failures.push(format!(
                "made {} tool calls, limit {limit}",
                metrics.tool_calls
            ));
        }
        if self.no_malformed && metrics.malformed_tool_calls > 0 {
            failures.push(format!(
                "{} malformed tool call(s)",
                metrics.malformed_tool_calls
            ));
        }

        for pattern in &self.final_matches {
            if !compile(pattern)?.is_match(&outcome.final_text) {
                failures.push(format!("final answer does not match /{pattern}/"));
            }
        }
        for pattern in &self.final_not_matches {
            if compile(pattern)?.is_match(&outcome.final_text) {
                failures.push(format!("final answer matches forbidden /{pattern}/"));
            }
        }

        if let Some(needle) = &self.semantic_hint_contains {
            match &metrics.semantic_hint {
                Some(hint) if hint.contains(needle.as_str()) => {}
                Some(hint) => {
                    failures.push(format!("semantic hint '{hint}' lacks '{needle}'"));
                }
                None => failures.push(format!("no semantic hint injected (wanted '{needle}')")),
            }
        }

        self.check_files(workspace, &mut failures)?;
        Ok(failures)
    }

    /// The expectations that look at what the agent actually produced. A task
    /// like "build me a page" can come back with a confident, detailed answer
    /// and an empty directory, so these are the ones that decide whether the
    /// work happened.
    fn check_files(&self, workspace: Option<&Path>, failures: &mut Vec<String>) -> AppResult<()> {
        let wants_files = !self.files_exist.is_empty()
            || !self.files_absent.is_empty()
            || !self.file_matches.is_empty();
        if !wants_files {
            return Ok(());
        }
        let Some(workspace) = workspace else {
            failures.push(
                "file expectations need `workspace_template` (nothing writable to inspect)"
                    .to_string(),
            );
            return Ok(());
        };

        for relative in &self.files_exist {
            if !workspace.join(relative).is_file() {
                failures.push(format!("file '{relative}' was not created"));
            }
        }
        for relative in &self.files_absent {
            if workspace.join(relative).exists() {
                failures.push(format!("file '{relative}' should not exist"));
            }
        }
        for expect in &self.file_matches {
            let path = workspace.join(&expect.path);
            let Ok(contents) = std::fs::read_to_string(&path) else {
                failures.push(format!("file '{}' is missing or unreadable", expect.path));
                continue;
            };
            let matched = compile(&expect.pattern)?.is_match(&contents);
            if matched == expect.absent {
                let verb = if expect.absent {
                    "matches forbidden"
                } else {
                    "does not match"
                };
                failures.push(format!("'{}' {verb} /{}/", expect.path, expect.pattern));
            }
        }
        Ok(())
    }
}

fn compile(pattern: &str) -> AppResult<regex::Regex> {
    regex::Regex::new(pattern)
        .map_err(|e| AppError::Custom(format!("bad expectation regex /{pattern}/: {e}")))
}

fn status_name(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::TimedOut => "timed_out",
        RunStatus::SetupFailed => "setup_failed",
    }
}

impl Scenario {
    /// The user turns, in order. Always at least one — `load` rejects a
    /// scenario that declares none.
    pub fn turns(&self) -> &[String] {
        &self.prompts
    }

    /// Parse and validate, without touching the filesystem. `origin` only
    /// prefixes error messages.
    pub fn from_toml(text: &str, origin: &str) -> AppResult<Self> {
        let mut scenario: Self =
            toml::from_str(text).map_err(|e| AppError::Custom(format!("{origin}: {e}")))?;
        match (scenario.prompt.take(), scenario.prompts.is_empty()) {
            (Some(_), false) => {
                return Err(AppError::Custom(format!(
                    "{origin}: set either `prompt` (one turn) or `prompts` (several), not both"
                )));
            }
            (Some(prompt), true) => scenario.prompts = vec![prompt],
            (None, true) => {
                return Err(AppError::Custom(format!(
                    "{origin}: needs `prompt = \"…\"` or `prompts = [\"…\", \"…\"]`"
                )));
            }
            (None, false) => {}
        }
        if let Some(index) = scenario.prompts.iter().position(|p| p.trim().is_empty()) {
            return Err(AppError::Custom(format!(
                "{origin}: prompts[{index}] is empty — every turn needs a user message"
            )));
        }
        Ok(scenario)
    }

    pub fn load(path: &Path) -> AppResult<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| AppError::Custom(format!("{}: {e}", path.display())))?;
        let mut scenario = Self::from_toml(&text, &path.display().to_string())?;
        // A workspace in the file is relative to the file, not to wherever
        // the harness happens to be invoked from.
        let base = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let resolve = |relative: PathBuf| {
            if relative.is_absolute() {
                relative
            } else {
                base.join(relative)
            }
        };
        if let Some(workspace) = scenario.workspace.take() {
            scenario.workspace = Some(resolve(workspace));
        }
        if let Some(template) = scenario.workspace_template.take() {
            scenario.workspace_template = Some(resolve(template));
        }
        if let Some(append) = scenario.system_prompt_append.take() {
            scenario.system_prompt_append = Some(resolve(append));
        }
        if scenario.workspace.is_some() && scenario.workspace_template.is_some() {
            return Err(AppError::Custom(format!(
                "{}: set either `workspace` or `workspace_template`, not both",
                path.display()
            )));
        }
        Ok(scenario)
    }
}

pub async fn run_one(args: ScenarioArgs, config_path: Option<PathBuf>) -> AppResult<i32> {
    let scenario = Scenario::load(&args.file)?;
    let report = execute(
        &scenario,
        args.repeat,
        &args.out,
        args.concurrency,
        config_path.as_deref(),
    )
    .await?;
    emit(&report, args.json)?;
    Ok(if report.passed {
        0
    } else {
        EXIT_EXPECTATIONS_FAILED
    })
}

pub async fn run_suite(args: SuiteArgs, config_path: Option<PathBuf>) -> AppResult<i32> {
    let files = collect_scenarios(&args.dir)?;
    if files.is_empty() {
        return Err(AppError::Custom(format!(
            "no .toml scenarios under {}",
            args.dir.display()
        )));
    }

    let mut scenarios = Vec::new();
    for file in &files {
        let scenario = Scenario::load(file)?;
        let report = execute(
            &scenario,
            args.repeat,
            &args.out,
            args.concurrency,
            config_path.as_deref(),
        )
        .await?;
        scenarios.push(report);
    }

    let suite = SuiteReport::new(scenarios);
    let path = args.out.join(format!("suite-{}.json", run_stamp()));
    write_json(&path, &suite)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&suite)?);
    } else {
        println!("{}", report::render_suite(&suite));
        println!("\nreport: {}", path.display());
    }
    Ok(if suite.passed {
        0
    } else {
        EXIT_EXPECTATIONS_FAILED
    })
}

/// Scenario files, sorted so a suite run is reproducible in order.
fn collect_scenarios(dir: &Path) -> AppResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .map_err(|e| AppError::Custom(format!("{}: {e}", current.display())))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "toml") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Run `repeat` children and assemble the scenario report.
async fn execute(
    scenario: &Scenario,
    repeat_override: Option<usize>,
    out: &Path,
    concurrency: usize,
    config_path: Option<&Path>,
) -> AppResult<ScenarioReport> {
    let repeats = repeat_override
        .or(scenario.repeat)
        .unwrap_or(crate::harness::DEFAULT_REPEATS)
        .max(1);
    let stamp = run_stamp();
    let dir = out.join(sanitize(&scenario.name)).join(&stamp);
    std::fs::create_dir_all(&dir)
        .map_err(|e| AppError::Custom(format!("{}: {e}", dir.display())))?;

    let permits = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut tasks = Vec::with_capacity(repeats);
    for index in 0..repeats {
        let permits = Arc::clone(&permits);
        let mut scenario = scenario.clone();
        let config_path = config_path.map(Path::to_path_buf);
        let trace_path = dir.join(format!("run-{index}.jsonl"));
        // A writable task gets its own copy of the template, kept next to the
        // trace so the files the agent produced can be read afterwards.
        let scratch = match &scenario.workspace_template {
            Some(template) => {
                let scratch = dir.join(format!("run-{index}-workspace"));
                copy_tree(template, &scratch)?;
                scenario.workspace = Some(scratch.clone());
                Some(scratch)
            }
            None => None,
        };
        tasks.push(tokio::spawn(async move {
            let _permit = permits.acquire().await;
            let outcome = spawn_run(&scenario, &trace_path, config_path.as_deref()).await;
            (index, trace_path, scratch, outcome)
        }));
    }

    let mut runs = Vec::with_capacity(repeats);
    for task in futures::future::join_all(tasks).await {
        let (index, trace_path, scratch, outcome) =
            task.map_err(|e| AppError::Custom(format!("scenario run task failed: {e}")))?;
        let outcome = outcome?;
        // A missing trace is itself a finding, not a hard error: the child
        // may have died before writing one.
        let trace = Trace::read(&trace_path).unwrap_or_default();
        let metrics = RunMetrics::from_trace(&trace);
        let failures = scenario
            .expect
            .check(&outcome, &metrics, scratch.as_deref())?;
        runs.push(RunReport {
            index,
            trace_path,
            workspace: scratch,
            outcome,
            metrics,
            failures,
        });
    }
    runs.sort_by_key(|run| run.index);

    let report = ScenarioReport::new(scenario, runs, dir.clone());
    write_json(&dir.join("report.json"), &report)?;
    Ok(report)
}

/// The child's whole command line. Every flag comes first and the turns last,
/// after `--`, so a prompt starting with `-` is never read as a flag.
fn exec_args(
    scenario: &Scenario,
    trace_path: &Path,
    config_path: Option<&Path>,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = Vec::new();

    // `--config` is a global flag, so it goes before the subcommand.
    if let Some(path) = config_path {
        push_flag(&mut args, "--config", path);
    }
    args.push("exec".into());
    args.push("--json".into());
    push_flag(&mut args, "--trace", trace_path);
    push_flag(&mut args, "--approve", &scenario.approve);
    push_flag(&mut args, "--timeout", scenario.timeout.to_string());
    push_flag(&mut args, "--semantic", &scenario.semantic);
    if let Some(workspace) = &scenario.workspace {
        push_flag(&mut args, "--workspace", workspace);
    }
    if let Some(append) = &scenario.system_prompt_append {
        push_flag(&mut args, "--system-append", append);
    }
    if let Some(answer) = &scenario.answer {
        push_flag(&mut args, "--answer", answer);
    }
    if let Some(max_steps) = scenario.max_steps {
        push_flag(&mut args, "--max-steps", max_steps.to_string());
    }
    if let Some(provider) = &scenario.provider {
        push_flag(&mut args, "--provider", provider);
    }
    if let Some(model) = &scenario.model {
        push_flag(&mut args, "--model", model);
    }
    if scenario.mcp {
        args.push("--mcp".into());
    }
    if scenario.save_session {
        args.push("--save-session".into());
    }

    args.push("--".into());
    for prompt in scenario.turns() {
        args.push(prompt.into());
    }
    args
}

fn push_flag(args: &mut Vec<std::ffi::OsString>, name: &str, value: impl AsRef<std::ffi::OsStr>) {
    args.push(name.into());
    args.push(value.as_ref().to_owned());
}

/// Run one repeat as `pooprusteek exec --json`, killing it if it outlives
/// its own timeout by a margin.
async fn spawn_run(
    scenario: &Scenario,
    trace_path: &Path,
    config_path: Option<&Path>,
) -> AppResult<RunOutcome> {
    let exe = std::env::current_exe()
        .map_err(|e| AppError::Custom(format!("cannot locate own binary: {e}")))?;

    let mut command = tokio::process::Command::new(exe);
    command
        .args(exec_args(scenario, trace_path, config_path))
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let child = command
        .spawn()
        .map_err(|e| AppError::Custom(format!("cannot spawn exec child: {e}")))?;

    // The child enforces `--timeout` itself; this is the backstop for a
    // child that wedges before its own timer can fire.
    let grace = Duration::from_secs(scenario.timeout + 60);
    let output = match tokio::time::timeout(grace, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return Ok(child_failed(
                trace_path,
                format!("exec child i/o error: {error}"),
            ));
        }
        Err(_) => {
            return Ok(child_failed(
                trace_path,
                format!("exec child killed after {}s", grace.as_secs()),
            ));
        }
    };

    let stdout = crate::util::decode_process_output(&output.stdout);
    match serde_json::from_str::<RunOutcome>(stdout.trim()) {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            let stderr = crate::util::decode_process_output(&output.stderr);
            Ok(child_failed(
                trace_path,
                format!(
                    "unreadable exec output ({error}); stderr: {}",
                    crate::util::truncate_with_ellipsis(stderr.trim(), 500)
                ),
            ))
        }
    }
}

/// A child that never reported an outcome is recorded as a setup failure so
/// the repeat still appears in the report instead of vanishing.
fn child_failed(trace_path: &Path, error: String) -> RunOutcome {
    RunOutcome {
        status: RunStatus::SetupFailed,
        trace_path: trace_path.to_path_buf(),
        duration_ms: 0,
        turns: 0,
        final_text: String::new(),
        tools: Vec::new(),
        sub_agents: 0,
        questions: Vec::new(),
        error: Some(error),
        session_id: None,
        semantic_ready: false,
    }
}

/// Recursive directory copy. `fs_extra` would do this in one call, but the
/// project's rule is to stay native rather than add a dependency for a dozen
/// lines. Symlinks are not followed — a template holding one is a mistake, and
/// silently dereferencing it would let a scenario escape its scratch dir.
fn copy_tree(from: &Path, to: &Path) -> AppResult<()> {
    if !from.is_dir() {
        return Err(AppError::Custom(format!(
            "workspace_template {} is not a directory",
            from.display()
        )));
    }
    std::fs::create_dir_all(to).map_err(|e| AppError::Custom(format!("{}: {e}", to.display())))?;
    let entries = std::fs::read_dir(from)
        .map_err(|e| AppError::Custom(format!("{}: {e}", from.display())))?;
    for entry in entries.flatten() {
        let target = to.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|e| AppError::Custom(format!("{}: {e}", entry.path().display())))?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| AppError::Custom(format!("{}: {e}", target.display())))?;
        }
    }
    Ok(())
}

fn emit(report: &ScenarioReport, json: bool) -> AppResult<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        println!("{}", report::render_scenario(report));
        println!("\nreport: {}", report.dir.join("report.json").display());
    }
    Ok(())
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError::Custom(format!("{}: {e}", parent.display())))?;
    }
    let json = serde_json::to_string_pretty(value)?;
    crate::util::atomic_write(path, json.as_bytes())
        .map_err(|e| AppError::Custom(format!("{}: {e}", path.display())))
}

/// Scenario names become directory names, so keep them to safe characters.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::driver::ToolInvocation;

    fn outcome(status: RunStatus, text: &str) -> RunOutcome {
        RunOutcome {
            status,
            trace_path: PathBuf::from("t.jsonl"),
            duration_ms: 10,
            turns: 1,
            final_text: text.to_string(),
            tools: vec![ToolInvocation {
                name: "bash".into(),
                approved: true,
                ok: true,
            }],
            sub_agents: 0,
            questions: Vec::new(),
            error: None,
            session_id: None,
            semantic_ready: false,
        }
    }

    fn metrics_with_tool(tool: &str, steps: usize) -> RunMetrics {
        let mut metrics = RunMetrics::default();
        metrics.tools_used.insert(tool.to_string(), 1);
        metrics.tool_calls = 1;
        metrics.steps = steps;
        metrics
    }

    #[test]
    fn satisfied_expectations_yield_no_failures() {
        let expect = Expect {
            status: Some("completed".into()),
            tools_used: vec!["bash".into()],
            tools_forbidden: vec!["task".into()],
            max_steps: Some(3),
            max_tool_calls: Some(2),
            no_malformed: true,
            final_matches: vec!["Cargo".into()],
            ..Expect::default()
        };
        let failures = expect
            .check(
                &outcome(RunStatus::Completed, "found Cargo.toml"),
                &metrics_with_tool("bash", 2),
                None,
            )
            .unwrap();
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn each_violation_is_reported_separately() {
        let expect = Expect {
            status: Some("completed".into()),
            tools_used: vec!["write".into()],
            tools_forbidden: vec!["bash".into()],
            max_steps: Some(1),
            no_malformed: true,
            final_not_matches: vec!["error".into()],
            semantic_hint_contains: Some("skill:x".into()),
            ..Expect::default()
        };
        let mut metrics = metrics_with_tool("bash", 5);
        metrics.malformed_tool_calls = 2;
        let failures = expect
            .check(
                &outcome(RunStatus::Failed, "an error happened"),
                &metrics,
                None,
            )
            .unwrap();
        assert_eq!(failures.len(), 7, "{failures:?}");
    }

    #[test]
    fn bad_regex_is_a_config_error_not_a_silent_pass() {
        let expect = Expect {
            final_matches: vec!["(unclosed".into()],
            ..Expect::default()
        };
        assert!(
            expect
                .check(
                    &outcome(RunStatus::Completed, "x"),
                    &RunMetrics::default(),
                    None
                )
                .is_err()
        );
    }

    #[test]
    fn file_expectations_read_the_real_workspace() {
        let dir = std::env::temp_dir().join("pooprusteek_scenario_files_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("css")).unwrap();
        std::fs::write(dir.join("index.html"), "<title>Todo</title>").unwrap();
        std::fs::write(dir.join("css/site.css"), "body { margin: 0 }").unwrap();

        let expect = Expect {
            files_exist: vec!["index.html".into(), "css/site.css".into()],
            files_absent: vec!["node_modules".into()],
            file_matches: vec![
                FileExpect {
                    path: "index.html".into(),
                    pattern: "<title>.+</title>".into(),
                    absent: false,
                },
                FileExpect {
                    path: "css/site.css".into(),
                    pattern: "TODO".into(),
                    absent: true,
                },
            ],
            ..Expect::default()
        };
        let passing = expect
            .check(
                &outcome(RunStatus::Completed, "done"),
                &RunMetrics::default(),
                Some(&dir),
            )
            .unwrap();
        assert!(passing.is_empty(), "{passing:?}");

        // A confident answer over an empty directory is the failure that
        // matters most, and it has to be reported per missing file.
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let failing = expect
            .check(
                &outcome(RunStatus::Completed, "I created the page"),
                &RunMetrics::default(),
                Some(&empty),
            )
            .unwrap();
        assert_eq!(failing.len(), 4, "{failing:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_expectations_without_a_writable_workspace_fail_loudly() {
        let expect = Expect {
            files_exist: vec!["index.html".into()],
            ..Expect::default()
        };
        let failures = expect
            .check(
                &outcome(RunStatus::Completed, "done"),
                &RunMetrics::default(),
                None,
            )
            .unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("workspace_template"), "{failures:?}");
    }

    #[test]
    fn unknown_scenario_field_is_rejected() {
        let toml = "name = \"x\"\nprompt = \"y\"\ntypo_field = 1\n";
        assert!(Scenario::from_toml(toml, "t.toml").is_err());
    }

    #[test]
    fn a_lone_prompt_is_one_turn() {
        let scenario = Scenario::from_toml("name = \"x\"\nprompt = \"y\"\n", "t.toml").unwrap();
        assert_eq!(scenario.turns().to_vec(), ["y"]);
    }

    #[test]
    fn a_prompt_list_is_several_turns_in_order() {
        let scenario = Scenario::from_toml(
            "name = \"x\"\nprompts = [\"first\", \"second\", \"third\"]\n",
            "t.toml",
        )
        .unwrap();
        assert_eq!(scenario.turns().to_vec(), ["first", "second", "third"]);
    }

    #[test]
    fn prompt_and_prompts_together_are_a_config_error() {
        let error = Scenario::from_toml(
            "name = \"x\"\nprompt = \"y\"\nprompts = [\"a\", \"b\"]\n",
            "t.toml",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("not both"), "{error}");
        assert!(error.contains("t.toml"), "{error}");
    }

    #[test]
    fn a_scenario_with_no_prompt_at_all_is_a_config_error() {
        for toml in ["name = \"x\"\n", "name = \"x\"\nprompts = []\n"] {
            let error = Scenario::from_toml(toml, "t.toml").unwrap_err().to_string();
            assert!(error.contains("prompts"), "{error}");
        }
    }

    #[test]
    fn an_empty_turn_is_a_config_error() {
        let error = Scenario::from_toml("name = \"x\"\nprompts = [\"a\", \"  \"]\n", "t.toml")
            .unwrap_err()
            .to_string();
        assert!(error.contains("prompts[1]"), "{error}");
    }

    #[test]
    fn every_turn_reaches_the_child_after_the_separator() {
        let scenario = Scenario::from_toml(
            "name = \"x\"\nprompts = [\"one\", \"two\", \"-three\"]\n",
            "t.toml",
        )
        .unwrap();
        let rendered: Vec<String> = exec_args(&scenario, Path::new("t.jsonl"), None)
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let separator = rendered
            .iter()
            .position(|arg| arg == "--")
            .expect("prompts are passed after `--`");
        // A prompt that looks like a flag must survive as a prompt.
        assert_eq!(rendered[separator + 1..].to_vec(), ["one", "two", "-three"]);
        assert_eq!(rendered.first().map(String::as_str), Some("exec"));
    }

    #[test]
    fn defaults_fill_in_for_a_minimal_scenario() {
        let scenario = Scenario::from_toml("name = \"x\"\nprompt = \"y\"\n", "t.toml").unwrap();
        assert_eq!(scenario.approve, "all");
        assert_eq!(scenario.semantic, "off");
        assert_eq!(scenario.timeout, 300);
        assert!(!scenario.mcp);
        assert_eq!(scenario.expect.min_pass_rate(), 1.0);
    }

    #[test]
    fn names_are_reduced_to_path_safe_directories() {
        assert_eq!(sanitize("rag: skills/mcp"), "rag--skills-mcp");
    }
}
