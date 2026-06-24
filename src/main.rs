mod app;
mod config;
mod error;
mod provider;
mod agent;
mod tools;
mod tui;

use color_eyre::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("pooprusteek=debug".parse()?))
        .init();

    let config = config::load().unwrap_or_default();
    let mut app = app::App::new(config).await?;
    app.run().await?;

    Ok(())
}
