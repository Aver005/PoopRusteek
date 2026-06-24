use crate::config::{Config, ProviderKind, ProviderConfig, save};
use crate::error::AppResult;

pub fn run_onboarding() -> AppResult<Config> {
    println!();
    println!("  Welcome to Pooprusteek!");
    println!("  A fast TUI coding agent powered by DeepSeek");
    println!();
    println!("  Let's set up your configuration.");
    println!();

    let token = prompt_token()?;
    let model = prompt_model()?;

    let config = Config {
        provider: ProviderConfig {
            kind: ProviderKind::Deepseek,
            token,
            model,
            base_url: None,
            temperature: 0.7,
            max_tokens: 4096,
        },
        ..Config::default()
    };

    save(&config)?;
    println!();
    println!("  Configuration saved. Press any key to start...");
    wait_for_key()?;

    Ok(config)
}

fn prompt_token() -> AppResult<String> {
    print!("  Enter your DeepSeek token: ");
    use std::io::Write;
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let token = input.trim().to_string();

    if token.is_empty() {
        println!("  Warning: No token provided. You'll need to configure it later.");
        println!("  Set it in ~/.config/pooprusteek/config.toml");
        return Ok(String::new());
    }

    Ok(token)
}

fn prompt_model() -> AppResult<String> {
    println!();
    println!("  Select model:");
    println!("    1) deepseek-chat (default)");
    println!("    2) deepseek-reasoner");
    println!();
    print!("  Choice [1]: ");
    use std::io::Write;
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;

    let model = match input.trim() {
        "2" => "deepseek-reasoner",
        _ => "deepseek-chat",
    };

    Ok(model.to_string())
}

fn wait_for_key() -> AppResult<()> {
    use std::io::Read;
    let mut buf = [0u8; 1];
    std::io::stdin().read_exact(&mut buf)?;
    Ok(())
}
