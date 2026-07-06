mod acp;
mod agent;
mod app;
mod cli;
mod commands;
mod config;
mod debug_log;
mod error;
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

    // Tracing goes to a file, never stdout/stderr: the TUI owns the
    // terminal (an INFO line mid-frame paints garbage over the interface)
    // and in --acp mode stdout is the JSON-RPC channel. Background tasks
    // (semantic init/backfill, MCP reconnects) log mid-session, so "only
    // startup logs" was never a safe assumption. Falls back to discarding
    // logs if the file can't be opened — silence beats a corrupted frame.
    let log_path = Config::data_dir().join("pooprusteek.log");
    let _ = std::fs::create_dir_all(Config::data_dir());
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("pooprusteek=info"));
    match log_file {
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        None => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::sink)
            .init(),
    }

    let args = Args::parse();
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
