mod acp;
mod agent;
mod app;
mod checkpoints;
mod cli;
mod commands;
mod config;
mod context;
mod debug_log;
mod error;
mod harness;
mod instructions;
mod logging;
mod mcp;
mod prompts;
mod provider;
mod safe_write;
mod semantic;
mod server;
mod session;
mod skills;
mod tools;
mod tui;
mod update;
mod util;
mod whitelist;

use clap::{Parser, Subcommand};
use color_eyre::Result;
use config::Config;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "pooprusteek")]
#[command(about = "A fast TUI coding agent powered by DeepSeek")]
struct Args {
    #[arg(long)]
    acp: bool,

    #[arg(long)]
    debug_log: bool,

    /// Start the TUI with the API server already running (same as typing
    /// `/serve on` right away). `--server` and `--api` are aliases.
    #[arg(long, visible_alias = "server", visible_alias = "api")]
    serve: bool,

    /// Run the API server without the TUI: stdout carries a timestamped
    /// event/request log instead of an interface. Same as `--api --uiless`.
    #[arg(long)]
    proxy: bool,

    /// With --serve/--server/--api: drop the TUI (headless proxy mode).
    #[arg(long, requires = "serve")]
    uiless: bool,

    /// Read config from this file instead of the user's real one. Mainly
    /// for harness runs against a throwaway token.
    #[arg(long, value_name = "FILE")]
    config: Option<std::path::PathBuf>,

    /// Headless test-harness subcommands (`exec`, `scenario`, `suite`,
    /// `mine`, `mock-provider`). Absent means the TUI, as before.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Top-level subcommands. Only the harness has any today; the TUI stays the
/// bare-invocation default so no existing usage changes.
#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Harness(harness::Command),
}

fn main() -> Result<()> {
    color_eyre::install()?;

    let args = Args::parse();

    // Tracing goes to files, never stdout/stderr: the TUI owns the terminal
    // (an INFO line mid-frame paints garbage over the interface) and in
    // --acp mode stdout is the JSON-RPC channel. `logging::setup` also opens
    // `errors.log` (ERROR-only + the in-UI red marker) and, in TUI mode,
    // redirects the raw stderr fd there so dependency output that bypasses
    // tracing (ONNX Runtime) can't corrupt a frame. Args are parsed first so
    // the redirect decision is made before anything that loads ONNX.
    // Harness subcommands own stdout the same way --acp and --proxy do, so
    // they must not get the TUI's stderr redirect either.
    let headless = args.acp || args.proxy || (args.serve && args.uiless) || args.command.is_some();
    logging::setup(!headless);

    debug_log::init(args.debug_log)?;
    let config: Config = match config::load_from_optional(args.config.as_deref()) {
        Ok(c) => c,
        // A config that cannot be read is fatal for anything non-interactive:
        // falling back to defaults would run against the wrong provider, or
        // against none at all. That cost real debugging time once — a CRLF
        // config template made the harness report "no provider configured"
        // while the actual cause was a TOML parse error two lines up. The TUI
        // keeps the lenient path so it can start and let the user fix it.
        Err(e) if args.config.is_some() || args.command.is_some() => {
            eprintln!("Error: {e}");
            std::process::exit(3);
        }
        Err(e) => {
            eprintln!("Warning: failed to load config, using defaults: {e}");
            Config::default()
        }
    };

    // До развилки по режимам: инструменты правки пишут журнал откатов в любом
    // из них, и папка у всех должна быть одна.
    checkpoints::Store::init(Config::data_dir());

    // A manual runtime instead of `#[tokio::main]` for one reason: bounded
    // shutdown. `spawn_blocking` work cannot be aborted, and the semantic
    // layer legitimately runs ONNX embedding there for seconds to minutes
    // (model init, MCP-corpus re-embed, history backfill). A plain runtime
    // drop waits for ALL blocking tasks, so quitting during that background
    // load left the process alive, holding the user's shell hostage.
    // `shutdown_timeout` abandons the blocking threads after a grace period
    // — safe here: every durable write goes through `util::atomic_write`,
    // the persist worker is flushed (bounded) before `run()` returns, and
    // the semantic index is a rebuildable cache that re-fills on the next
    // launch. Async tasks (MCP startup connects etc.) are cancelled by the
    // shutdown as they always were.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run_async(args, config));
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    match result {
        // A harness subcommand's exit code is its verdict (failed run,
        // unmet expectations), not an error to print a backtrace for.
        Ok(Some(code)) if code != 0 => std::process::exit(code),
        Ok(_) => Ok(()),
        Err(error) => Err(error),
    }
}

/// `Ok(Some(code))` carries a harness subcommand's exit code; `Ok(None)`
/// means a normal exit.
async fn run_async(args: Args, config: Config) -> Result<Option<i32>> {
    // Harness subcommands are checked first: they never build an App and
    // must not be reordered behind the TUI's terminal setup.
    if let Some(Command::Harness(command)) = args.command {
        return Ok(Some(harness::run(command, config, args.config).await?));
    }

    // ACP dispatch stays inside the runtime: the server uses
    // `block_in_place` internally, which needs a live multi-thread runtime.
    if args.acp {
        run_acp_server(&config)?;
        return Ok(None);
    }

    // Headless proxy: the API server is the whole program — no TUI, stdout
    // is the event log (tracing still goes to the data-dir file).
    if args.proxy || (args.serve && args.uiless) {
        server::proxy::run(config).await?;
        return Ok(None);
    }

    // TUI-only (never in --acp mode, where stdout is the JSON-RPC channel):
    // name the window before the slow parts of startup (MCP connects) run;
    // the render loop takes over the title from the first frame on.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::SetTitle("Starting...")
    );

    let mut app = app::App::new(config).await?;
    if args.serve {
        app.start_server("--serve flag");
    }
    app.run().await?;

    Ok(None)
}

fn run_acp_server(config: &Config) -> Result<()> {
    if config.provider.token.is_empty() {
        eprintln!("Error: No token configured. Run without --acp to set up.");
        std::process::exit(1);
    }

    let provider = crate::provider::deepseek::DeepseekProvider::new(
        &config.provider,
        config.agent.rate_limit_ms,
        config.agent.rate_limit_per_minute,
        config.agent.max_retries,
    )?;
    let provider = Arc::new(provider);
    let mut server = acp::server::AcpServer::new(provider);
    server.run()?;
    Ok(())
}
