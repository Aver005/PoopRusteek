mod acp;
mod agent;
mod app;
mod checkpoints;
mod cli;
mod clipboard;
mod commands;
mod config;
mod context;
mod debug_log;
mod error;
#[expect(
    dead_code,
    reason = "контракты шлюза: потребители появятся в следующих дорожках"
)]
mod gateway;
mod harness;
mod instructions;
mod logging;
mod mcp;
mod prompts;
mod provider;
mod safe_write;
mod secrets;
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

#[derive(Parser)]
#[command(name = "pooprusteek", version)]
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

    /// Keep sessions, logs, the semantic index and the rollback journal in
    /// this directory instead of the user's real data dir. Harness runs need
    /// it: `dirs::data_dir()` has no env override on Windows, so without it a
    /// test run writes into live user data.
    #[arg(long, value_name = "DIR")]
    data_dir: Option<std::path::PathBuf>,

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
    /// Manage secrets in the system keyring (values are never printed).
    Secrets(cli::secrets::SecretsArgs),
    /// Show where config values and secrets come from and what is wrong.
    Doctor(cli::doctor::DoctorArgs),
    /// Confirm a project's .pooprusteek config that wants to change where requests go.
    Trust(cli::trust::TrustArgs),
}

/// Ошибка аргументов — это `setup_failed` (код 3). Код 2 от clap совпадал с
/// `timed_out`, а `exec --json` оставался без JSON.
fn exit_on_usage_error(error: clap::Error) -> ! {
    use clap::error::ErrorKind;
    if matches!(
        error.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        error.exit();
    }
    // `args_os`: не-Unicode аргумент не должен ронять и сам отчёт об ошибке.
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if argv.iter().any(|arg| arg == "exec") && argv.iter().any(|arg| arg == "--json") {
        let outcome = harness::driver::RunOutcome::setup_failed(
            std::path::PathBuf::new(),
            error.to_string().trim().to_string(),
        );
        if let Ok(json) = serde_json::to_string_pretty(&outcome) {
            println!("{json}");
        }
    }
    let _ = error.print();
    std::process::exit(3);
}

fn main() -> Result<()> {
    color_eyre::install()?;

    let args = Args::try_parse().unwrap_or_else(|error| exit_on_usage_error(error));
    update::register_running_instance();

    // Раньше всех: и журнал, и хранилище откатов читают `Config::data_dir()`
    // на старте, а перенаправить их после этого уже нечем.
    if let Some(dir) = &args.data_dir
        && let Err(error) = config::set_data_dir(dir)
    {
        eprintln!("Error: {error}");
        std::process::exit(3);
    }
    // Флаги изоляции задают и конфиг: без них — перенос старой папки данных.
    if let Some(path) = &args.config {
        config::mark_explicit_config(path);
    }
    let mut startup_notices = config::choose_data_dir();

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

    // `secrets` и `doctor` не требуют рабочего конфига: сломанный файл — как раз их случай.
    if matches!(
        args.command,
        Some(Command::Secrets(_)) | Some(Command::Doctor(_)) | Some(Command::Trust(_))
    ) {
        let code = match args.command {
            Some(Command::Secrets(secrets_args)) => cli::secrets::run(secrets_args),
            Some(Command::Doctor(doctor_args)) => {
                cli::doctor::run(doctor_args, args.config.as_deref())
            }
            Some(Command::Trust(trust_args)) => cli::trust::run(trust_args, args.config.as_deref()),
            _ => 0,
        };
        std::process::exit(code);
    }

    // Подкоманды харнесса пишут трассу сами (`exec --trace`); общий `--debug-log`
    // открыл бы журнал раньше, и трасса ушла бы не в тот файл.
    if args.command.is_none() {
        debug_log::init(args.debug_log)?;
    }
    let mut config: Config = match config::load_from_optional(args.config.as_deref()) {
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
            // Не «первый запуск»: файл есть, но не прочитан. Онбординг поверх него не нужен,
            // а сохранение затёрло бы то, что человек правил руками.
            let mut fallback = Config::default();
            fallback.source.blocked = Some(e.to_string());
            fallback
        }
    };

    // Заметки о переносе данных и секретов показывает `App::new` один раз.
    startup_notices.append(&mut config.migration_notices);
    config.migration_notices = startup_notices;
    // Все ссылки прочитаны: копия файла перед миграцией больше не хранит ключи.
    if config.source.unresolved.is_empty() {
        config::after_verified_load(&Config::global_path());
    }

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
        let globals = harness::GlobalFlags {
            config: args.config,
            data_dir: args.data_dir,
        };
        return Ok(Some(harness::run(command, config, globals).await?));
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
    // Как в приложении: шлюз собирается вне цикла, запрос в это время ждёт готовности.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (gateway, mut pending) = app::start_gateway(config, &event_tx);
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            if let (app::events::AppEvent::GatewayBuilt(built), Some(waiting)) =
                (event, pending.take())
            {
                match built.0 {
                    Ok(ready) => waiting.fulfil(ready),
                    Err(reason) => waiting.fail(reason),
                }
            }
        }
    });
    let Some(provider) = gateway::handle::main_provider(config, Some(&gateway)) else {
        eprintln!("Error: No token configured. Run without --acp to set up.");
        std::process::exit(1);
    };
    let mut server = acp::server::AcpServer::new(provider);
    server.run()?;
    Ok(())
}
