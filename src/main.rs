mod acp;
mod agent;
mod app;
mod cli;
mod commands;
mod config;
mod debug_log;
mod error;
mod logging;
mod mcp;
mod prompts;
mod provider;
mod semantic;
mod server;
mod session;
mod skills;
mod tools;
mod tui;
mod util;
mod whitelist;

use clap::Parser;
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
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let args = Args::parse();

    // Tracing goes to files, never stdout/stderr: the TUI owns the terminal
    // (an INFO line mid-frame paints garbage over the interface) and in
    // --acp mode stdout is the JSON-RPC channel. `logging::setup` also opens
    // `errors.log` (ERROR-only + the in-UI red marker) and, in TUI mode,
    // redirects the raw stderr fd there so dependency output that bypasses
    // tracing (ONNX Runtime) can't corrupt a frame. Args are parsed first so
    // the redirect decision is made before anything that loads ONNX.
    let headless = args.acp || args.proxy || (args.serve && args.uiless);
    logging::setup(!headless);

    debug_log::init(args.debug_log)?;
    let config: Config = match config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Warning: failed to load config, using defaults: {e}");
            Config::default()
        }
    };

    if args.acp {
        return run_acp_server(&config);
    }

    // Headless proxy: the API server is the whole program — no TUI, stdout
    // is the event log (tracing still goes to the data-dir file).
    if args.proxy || (args.serve && args.uiless) {
        return Ok(server::proxy::run(config).await?);
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

    Ok(())
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
