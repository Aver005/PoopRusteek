mod app;
mod config;
mod error;
mod provider;
mod agent;
mod tools;
mod tui;
mod mcp;

use color_eyre::Result;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("pooprusteek=info")),
        )
        .init();

    let config = config::load().unwrap_or_default();
    let mut app = app::App::new(config).await?;
    app.run().await?;

    Ok(())
}
