use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use tomato_router::config::is_loopback_listen;
use tomato_router::{Config, build_app, codex, server, state::AppState, tui};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "tomato-router",
    version,
    about = "Strict-priority OpenAI Responses gateway"
)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(
        short,
        long,
        default_value = "config/tomato.toml",
        env = "TOMATO_CONFIG"
    )]
    config: PathBuf,

    /// Validate the configuration, print a summary, and exit.
    #[arg(long)]
    check: bool,

    /// Override `server.listen` from the configuration file.
    #[arg(long)]
    listen: Option<String>,

    /// Log filter (overrides RUST_LOG).
    #[arg(long, env = "RUST_LOG")]
    log: Option<String>,

    /// Emit JSON logs instead of human-readable logs.
    #[arg(long)]
    log_json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Synchronize the local Tomato Router provider into Codex config.toml.
    Codex(CodexCommand),

    /// Open the interactive terminal console.
    Tui(TuiArgs),
}

#[derive(Debug, Args)]
struct TuiArgs {
    /// Start the gateway immediately after the TUI opens.
    #[arg(long)]
    autostart: bool,
}

#[derive(Debug, Args)]
struct CodexCommand {
    #[command(subcommand)]
    command: CodexSubcommand,
}

#[derive(Debug, Subcommand)]
enum CodexSubcommand {
    /// Update Codex's user config while preserving unrelated settings.
    Sync(CodexSyncArgs),
}

#[derive(Debug, Args)]
struct CodexSyncArgs {
    /// Explicit path to Codex config.toml (also supports a WSL UNC path).
    #[arg(long)]
    codex_config: Option<PathBuf>,

    /// Codex home directory containing config.toml.
    #[arg(long)]
    codex_home: Option<PathBuf>,

    /// Codex provider id to update.
    #[arg(long, default_value = "tomato")]
    provider: String,

    /// Default Codex model. Defaults to the first routed or mapped model.
    #[arg(long)]
    model: Option<String>,

    /// Client-facing gateway URL. Defaults to server.listen plus /v1.
    #[arg(long)]
    base_url: Option<String>,

    /// Show the planned update without writing or creating a backup.
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(Command::Tui(args)) = &cli.command {
        return tui::run(tui::Options {
            config_path: cli.config.clone(),
            listen_override: cli.listen.clone(),
            log_filter: cli.log.clone(),
            autostart: args.autostart,
        })
        .await;
    }

    init_tracing(cli.log.as_deref(), cli.log_json);

    let mut config = Config::load(&cli.config)?;
    if let Some(listen) = &cli.listen {
        config.server.listen = listen.clone();
    }

    if let Some(Command::Codex(CodexCommand {
        command: CodexSubcommand::Sync(args),
    })) = cli.command
    {
        let dry_run = args.dry_run;
        let plan = codex::sync(
            &config,
            &codex::SyncOptions {
                codex_config: args.codex_config,
                codex_home: args.codex_home,
                provider_id: args.provider,
                model: args.model,
                base_url: args.base_url,
                dry_run,
            },
        )?;
        println!(
            "Codex config {}: {}",
            if dry_run { "would update" } else { "updated" },
            plan.path.display()
        );
        println!(
            "provider={} model={} base_url={} wire_api=responses env_key={}",
            plan.provider_id,
            plan.model,
            plan.base_url,
            codex::DEFAULT_ENV_KEY
        );
        if let Some(backup) = plan.backup {
            println!("backup={}", backup.display());
        }
        if !plan.changed {
            println!("no changes needed");
        }
        return Ok(());
    }

    if cli.check {
        print_config_summary(&config);
        return Ok(());
    }

    let listen = config.server.listen.clone();
    let require_auth = config.server.require_auth;
    if !require_auth && !is_loopback_listen(&listen) {
        tracing::warn!(
            %listen,
            "server is listening on a non-loopback address without local authentication"
        );
    }

    let state = AppState::new(config)?;
    let app = build_app(state);

    let (listener, actual) = server::bind(&listen).await?;

    tracing::info!(
        listen = %actual,
        version = env!("CARGO_PKG_VERSION"),
        "tomato-router listening"
    );

    server::serve(listener, app, shutdown_signal()).await?;

    tracing::info!("tomato-router stopped");
    Ok(())
}

fn init_tracing(filter: Option<&str>, json: bool) {
    let filter = filter
        .map(str::to_string)
        .or_else(|| std::env::var("RUST_LOG").ok())
        .unwrap_or_else(|| "tomato_router=info,tower_http=warn".to_string());
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));

    let builder = tracing_subscriber::fmt().with_env_filter(env_filter);
    if json {
        builder.json().flatten_event(true).init();
    } else {
        builder.init();
    }
}

fn print_config_summary(config: &Config) {
    for line in config.summary_lines() {
        println!("{line}");
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to install Ctrl+C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
