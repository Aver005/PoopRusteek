//! Headless test harness — the CLI surface for driving the agent without a
//! terminal.
//!
//! Why it exists: unit tests cover code, but the interesting failures of a
//! coding agent are behavioural (a malformed tool call it can't recover
//! from, a RAG hint that points at the wrong skill, a turn that stops one
//! step early), and until now the only way to produce one was for a human
//! to sit at the TUI. `--acp` is a bare prompt relay with no tools, and the
//! `--proxy` API server is a provider gateway that explicitly ignores tool
//! calls — neither runs the agent loop.
//!
//! Subcommands:
//! - `exec` — one turn (or several, in one conversation), one JSONL trace
//!   ([`driver`]).
//! - `scenario` / `suite` — a turn repeated N times with expectations, since
//!   one sample of a nondeterministic model proves nothing ([`scenario`]).
//! - `mine` — bucket failure patterns across traces and saved sessions
//!   ([`mine`]).
//! - `mock-provider` — a scripted OpenAI-compatible endpoint, so agent-loop
//!   behaviour can be tested without a live model ([`mock`]).

pub mod driver;
pub mod metrics;
pub mod mine;
pub mod mock;
pub mod report;
pub mod scenario;
pub mod trace;

use crate::config::Config;
use crate::error::AppResult;
use clap::{Args, Subcommand};
use driver::{ApprovePolicy, ExecOptions, SemanticMode};
use std::path::PathBuf;
use std::time::Duration;

/// Where traces and reports land by default. Under `.dev/`, which is
/// git-ignored, so runs never dirty the tree.
/// Repeats when neither the command line nor the scenario file says.
pub const DEFAULT_REPEATS: usize = 3;

const DEFAULT_OUT_DIR: &str = ".dev/harness";

#[derive(Subcommand)]
pub enum Command {
    /// Run one agent turn headlessly and write a JSONL trace.
    Exec(ExecArgs),
    /// Run one scenario file, repeated, and report aggregated metrics.
    Scenario(ScenarioArgs),
    /// Run every scenario in a directory.
    Suite(SuiteArgs),
    /// Bucket failure patterns across traces and saved sessions.
    Mine(MineArgs),
    /// Serve a scripted OpenAI-compatible endpoint for deterministic runs.
    MockProvider(MockArgs),
}

#[derive(Args)]
pub struct ExecArgs {
    /// The user message to send. Give several and they run as several turns
    /// in one conversation, sharing history and the provider session.
    #[arg(required = true, num_args = 1..)]
    pub prompts: Vec<String>,

    /// Working directory for the turn (tools run here).
    #[arg(long, short = 'C')]
    pub workspace: Option<PathBuf>,

    /// Trace file. Defaults to `.dev/harness/<timestamp>.jsonl`.
    #[arg(long)]
    pub trace: Option<PathBuf>,

    /// Tool-approval policy: `all`, `none`, `whitelist`, or `except:a,b`.
    #[arg(long, default_value = "all")]
    pub approve: ApprovePolicy,

    /// Canned reply to the `question` tool (default: its first option).
    #[arg(long)]
    pub answer: Option<String>,

    #[arg(long)]
    pub max_steps: Option<usize>,

    /// Wall-clock budget in seconds; the turn is aborted past it.
    #[arg(long, default_value_t = 300)]
    pub timeout: u64,

    /// Semantic layer: `off`, `background`, or `ready[:seconds]` to wait for
    /// the index before the turn starts.
    #[arg(long, default_value = "off")]
    pub semantic: String,

    /// File whose contents are appended to the assembled system prompt.
    /// The point of the harness is comparing behaviour, and prompt wording is
    /// the cheapest thing to change, so a variant is a file on disk rather
    /// than a string on a command line: it stays readable and diffable.
    #[arg(long)]
    pub system_append: Option<PathBuf>,

    /// Connect configured MCP servers first (slow, and depends on hosts
    /// outside the sandbox).
    #[arg(long)]
    pub mcp: bool,

    /// `/providers` entry to run against; `deepseek` is the built-in client.
    #[arg(long)]
    pub provider: Option<String>,

    #[arg(long)]
    pub model: Option<String>,

    /// Save the turn as a session file (feeds `mine` and the history index).
    #[arg(long)]
    pub save_session: bool,

    /// Print the outcome as JSON instead of a human summary.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct ScenarioArgs {
    /// Scenario TOML file.
    pub file: PathBuf,

    /// How many times to run it. A single sample of a nondeterministic
    /// model is not evidence.
    /// Repeats per scenario. Overrides a scenario file's own `repeat`;
    /// absent means the file decides, and [`DEFAULT_REPEATS`] if it does not.
    #[arg(long)]
    pub repeat: Option<usize>,

    /// Directory for traces and the report.
    #[arg(long, default_value = DEFAULT_OUT_DIR)]
    pub out: PathBuf,

    /// Run repeats concurrently. Off by default: parallel turns share the
    /// provider's rate limit and make latency numbers meaningless.
    #[arg(long, default_value_t = 1)]
    pub concurrency: usize,

    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct SuiteArgs {
    /// Directory of scenario TOML files (searched recursively).
    pub dir: PathBuf,

    /// Repeats per scenario. Overrides a scenario file's own `repeat`;
    /// absent means the file decides, and [`DEFAULT_REPEATS`] if it does not.
    #[arg(long)]
    pub repeat: Option<usize>,

    #[arg(long, default_value = DEFAULT_OUT_DIR)]
    pub out: PathBuf,

    #[arg(long, default_value_t = 1)]
    pub concurrency: usize,

    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct MineArgs {
    /// Trace files or directories of them. Defaults to `.dev/harness`.
    pub paths: Vec<PathBuf>,

    /// Also mine the saved session corpus in the data dir.
    #[arg(long)]
    pub sessions: bool,

    /// How many entries to show per bucket.
    #[arg(long, default_value_t = 10)]
    pub top: usize,

    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct MockArgs {
    #[arg(long, default_value_t = 811)]
    pub port: u16,

    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Script file: canned replies, in order, per request. Without one the
    /// endpoint echoes a fixed acknowledgement.
    #[arg(long)]
    pub script: Option<PathBuf>,
}

/// Dispatch a harness subcommand. Returns the process exit code so callers
/// can propagate a scenario failure to CI without an extra error type.
///
/// `config_path` is the global `--config`, if any: the scenario runner has to
/// hand it to the `exec` children it spawns, or they would load the user's
/// real config instead of the one this run was pointed at.
pub async fn run(command: Command, config: Config, config_path: Option<PathBuf>) -> AppResult<i32> {
    match command {
        Command::Exec(args) => exec(args, config).await,
        Command::Scenario(args) => scenario::run_one(args, config_path).await,
        Command::Suite(args) => scenario::run_suite(args, config_path).await,
        Command::Mine(args) => mine::run(args),
        Command::MockProvider(args) => mock::run(args).await,
    }
}

async fn exec(args: ExecArgs, config: Config) -> AppResult<i32> {
    let options = ExecOptions {
        prompts: args.prompts,
        workspace: args.workspace,
        trace_path: args.trace.unwrap_or_else(default_trace_path),
        approve: args.approve,
        answer: args.answer,
        max_steps: args.max_steps,
        timeout: Duration::from_secs(args.timeout),
        semantic: parse_semantic(&args.semantic)?,
        mcp: args.mcp,
        provider: args.provider,
        model: args.model,
        save_session: args.save_session,
        system_append: args.system_append.clone(),
    };

    let outcome = driver::exec(config, options).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        println!("{}", report::render_run(&outcome));
    }
    Ok(outcome.status.exit_code())
}

/// `off` | `background` | `ready` | `ready:<seconds>`
fn parse_semantic(value: &str) -> AppResult<SemanticMode> {
    let value = value.trim();
    if value == "off" {
        return Ok(SemanticMode::Off);
    }
    if value == "background" {
        return Ok(SemanticMode::Background);
    }
    if let Some(rest) = value.strip_prefix("ready") {
        let seconds = match rest.strip_prefix(':') {
            Some(number) => number.trim().parse::<u64>().map_err(|_| {
                crate::error::AppError::Custom(format!("bad ready budget '{number}'"))
            })?,
            // A cold first run downloads ~120 MB of model, so the default
            // budget is generous rather than tuned.
            None => 600,
        };
        return Ok(SemanticMode::Ready(Duration::from_secs(seconds)));
    }
    Err(crate::error::AppError::Custom(format!(
        "unknown semantic mode '{value}' (off | background | ready[:seconds])"
    )))
}

fn default_trace_path() -> PathBuf {
    PathBuf::from(DEFAULT_OUT_DIR).join(format!("{}.jsonl", run_stamp()))
}

/// Filesystem-safe timestamp, matching the session-id shape used elsewhere.
pub fn run_stamp() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H-%M-%S-%3fZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_modes_parse() {
        assert_eq!(parse_semantic("off").unwrap(), SemanticMode::Off);
        assert_eq!(
            parse_semantic("background").unwrap(),
            SemanticMode::Background
        );
        assert_eq!(
            parse_semantic("ready:30").unwrap(),
            SemanticMode::Ready(Duration::from_secs(30))
        );
        assert!(matches!(
            parse_semantic("ready").unwrap(),
            SemanticMode::Ready(_)
        ));
        assert!(parse_semantic("ready:soon").is_err());
        assert!(parse_semantic("maybe").is_err());
    }

    #[test]
    fn run_stamp_is_path_safe() {
        let stamp = run_stamp();
        assert!(!stamp.contains(':'), "colons break Windows paths: {stamp}");
        assert!(stamp.ends_with('Z'));
    }
}
