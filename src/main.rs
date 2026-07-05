mod app;
mod config;
mod debug_log;
mod error;
mod provider;
mod agent;
mod tools;
mod tui;
mod mcp;
mod commands;
mod session;
mod cli;
mod acp;
mod prompts;
mod whitelist;
mod util;
mod skills;
mod semantic;

use std::sync::Arc;
use color_eyre::Result;
use clap::Parser;
use config::Config;

#[derive(Parser)]
#[command(name = "pooprusteek")]
#[command(about = "A fast TUI coding agent powered by DeepSeek")]
struct Args {
    #[arg(long)]
    acp: bool,

    #[arg(long)]
    debug_log: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("pooprusteek=info")),
        )
        .init();

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

    // TUI-only (never in --acp mode, where stdout is the JSON-RPC channel):
    // name the window before the slow parts of startup (MCP connects) run;
    // the render loop takes over the title from the first frame on.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle("Starting..."));

    let mut app = app::App::new(config).await?;
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
