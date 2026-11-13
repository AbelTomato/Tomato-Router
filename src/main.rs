use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tomato_router::{Config, build_app, state::AppState};
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.log.as_deref(), cli.log_json);

    let mut config = Config::load(&cli.config)?;
    if let Some(listen) = &cli.listen {
        config.server.listen = listen.clone();
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

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("failed to bind {listen}"))?;
    let actual = listener
        .local_addr()
        .with_context(|| "failed to read local address")?;

    tracing::info!(
        listen = %actual,
        version = env!("CARGO_PKG_VERSION"),
        "tomato-router listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP server failed")?;

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
    println!(
        "config OK: {} provider(s), {} route(s), listen={}",
        config.providers.len(),
        config.routes.len(),
        config.server.listen
    );
    println!(
        "local auth: {}",
        if config.server.require_auth {
            format!("required ({} key(s))", config.server.api_keys.len())
        } else {
            "disabled".to_string()
        }
    );
    for (id, ok, detail) in config.provider_env_status() {
        println!(
            "provider {id}: auth={} ({detail})",
            if ok { "ok" } else { "MISSING" }
        );
    }
    for route in &config.routes {
        println!("route {} -> {}", route.model, route.chain.join(" -> "));
    }
    if config.routes.is_empty() {
        let order = config
            .providers
            .iter()
            .map(|provider| provider.id.as_str())
            .collect::<Vec<_>>()
            .join(" -> ");
        println!("default chain (declaration order): {order}");
    }
}

fn is_loopback_listen(listen: &str) -> bool {
    let host = listen
        .rsplit_once(':')
        .map(|(host, _)| host.trim_matches(['[', ']']))
        .unwrap_or(listen);
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
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
