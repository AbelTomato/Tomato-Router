//! In-process gateway supervisor used by the TUI.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::server;
use crate::state::AppState;

#[derive(Debug)]
pub enum Command {
    Start {
        request_id: u64,
        config_path: PathBuf,
        listen_override: Option<String>,
    },
    Stop {
        request_id: u64,
    },
    Restart {
        request_id: u64,
        config_path: PathBuf,
        listen_override: Option<String>,
    },
    Shutdown,
}

pub enum ServiceEvent {
    Starting {
        request_id: u64,
    },
    Running {
        request_id: u64,
        state: AppState,
        listen: SocketAddr,
    },
    Stopping {
        request_id: u64,
        active_requests: usize,
    },
    Stopped {
        request_id: u64,
        forced: bool,
        message: Option<String>,
    },
    Failed {
        request_id: u64,
        message: String,
    },
}

struct Running {
    request_id: u64,
    state: AppState,
    listen: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    join: JoinHandle<Result<()>>,
}

pub async fn run(
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<ServiceEvent>,
    drain_timeout: Duration,
) {
    let (exit_tx, mut exit_rx) = mpsc::unbounded_channel::<Result<(), String>>();
    let mut running: Option<Running> = None;

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    if running.is_some() {
                        stop_running(
                            &mut running,
                            &mut exit_rx,
                            drain_timeout,
                            &events,
                            0,
                        )
                        .await;
                    }
                    break;
                };
                match command {
                    Command::Start {
                        request_id,
                        config_path,
                        listen_override,
                    } => {
                        if let Some(server) = &running {
                            let _ = events.send(ServiceEvent::Running {
                                request_id,
                                state: server.state.clone(),
                                listen: server.listen,
                            });
                            continue;
                        }
                        if let Some(server) = start_running(
                            &events,
                            &exit_tx,
                            request_id,
                            config_path,
                            listen_override,
                        )
                        .await
                        {
                            running = Some(server);
                        }
                    }
                    Command::Stop { request_id } => {
                        if running.is_none() {
                            let _ = events.send(ServiceEvent::Stopped {
                                request_id,
                                forced: false,
                                message: None,
                            });
                            continue;
                        }
                        stop_running(
                            &mut running,
                            &mut exit_rx,
                            drain_timeout,
                            &events,
                            request_id,
                        )
                        .await;
                    }
                    Command::Restart {
                        request_id,
                        config_path,
                        listen_override,
                    } => {
                        if running.is_some() {
                            stop_running(
                                &mut running,
                                &mut exit_rx,
                                drain_timeout,
                                &events,
                                request_id,
                            )
                            .await;
                        }
                        if let Some(server) = start_running(
                            &events,
                            &exit_tx,
                            request_id,
                            config_path,
                            listen_override,
                        )
                        .await
                        {
                            running = Some(server);
                        }
                    }
                    Command::Shutdown => {
                        if running.is_some() {
                            stop_running(
                                &mut running,
                                &mut exit_rx,
                                drain_timeout,
                                &events,
                                0,
                            )
                            .await;
                        }
                        break;
                    }
                }
            }
            exit = exit_rx.recv(), if running.is_some() => {
                let request_id = running.as_ref().map(|server| server.request_id).unwrap_or(0);
                running = None;
                let message = match exit {
                    Some(Ok(())) => "server stopped unexpectedly".to_string(),
                    Some(Err(error)) => format!("server stopped unexpectedly: {error}"),
                    None => "server task ended unexpectedly".to_string(),
                };
                tracing::warn!(%message, "TUI supervisor lost the HTTP server");
                let _ = events.send(ServiceEvent::Failed { request_id, message });
            }
        }
    }

    if running.is_some() {
        stop_running(&mut running, &mut exit_rx, drain_timeout, &events, 0).await;
    }
}

async fn start_running(
    events: &mpsc::UnboundedSender<ServiceEvent>,
    exit_tx: &mpsc::UnboundedSender<Result<(), String>>,
    request_id: u64,
    config_path: PathBuf,
    listen_override: Option<String>,
) -> Option<Running> {
    let _ = events.send(ServiceEvent::Starting { request_id });
    match start_server(request_id, config_path, listen_override, exit_tx.clone()).await {
        Ok(server) => {
            tracing::info!(
                listen = %server.listen,
                "gateway started from TUI"
            );
            let _ = events.send(ServiceEvent::Running {
                request_id,
                state: server.state.clone(),
                listen: server.listen,
            });
            Some(server)
        }
        Err(error) => {
            let message = format!("{error:#}");
            tracing::error!(%message, "failed to start gateway from TUI");
            let _ = events.send(ServiceEvent::Failed {
                request_id,
                message,
            });
            None
        }
    }
}

async fn start_server(
    request_id: u64,
    config_path: PathBuf,
    listen_override: Option<String>,
    exit_tx: mpsc::UnboundedSender<Result<(), String>>,
) -> Result<Running> {
    let mut config = Config::load(&config_path)?;
    if let Some(listen) = listen_override {
        config.server.listen = listen;
    }
    let listen = config.server.listen.clone();
    let state = AppState::new(config)?;
    let (listener, actual) = server::bind(&listen)
        .await
        .with_context(|| format!("cannot start gateway on `{listen}`"))?;
    let app = crate::build_app(state.clone());
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let join = tokio::spawn(async move {
        let result = server::serve(listener, app, async move {
            let _ = shutdown_rx.await;
        })
        .await;
        let _ = exit_tx.send(
            result
                .as_ref()
                .map(|_| ())
                .map_err(|error| format!("{error:#}")),
        );
        result
    });

    Ok(Running {
        request_id,
        state,
        listen: actual,
        shutdown: Some(shutdown_tx),
        join,
    })
}

async fn stop_running(
    running: &mut Option<Running>,
    exit_rx: &mut mpsc::UnboundedReceiver<Result<(), String>>,
    drain_timeout: Duration,
    events: &mpsc::UnboundedSender<ServiceEvent>,
    request_id: u64,
) {
    let Some(mut server) = running.take() else {
        let _ = events.send(ServiceEvent::Stopped {
            request_id,
            forced: false,
            message: None,
        });
        return;
    };

    let active_requests: usize = server
        .state
        .providers
        .all()
        .iter()
        .map(|provider| provider.inflight() + provider.queue_depth())
        .sum();
    let _ = events.send(ServiceEvent::Stopping {
        request_id,
        active_requests,
    });
    tracing::info!(active_requests, "stopping gateway from TUI");

    if let Some(shutdown) = server.shutdown.take() {
        let _ = shutdown.send(());
    }

    let mut message = None;
    let forced = match tokio::time::timeout(drain_timeout, exit_rx.recv()).await {
        Ok(Some(Ok(()))) => false,
        Ok(Some(Err(error))) => {
            message = Some(format!("server stopped with an error: {error:#}"));
            false
        }
        Ok(None) => {
            message = Some("server task channel closed".to_string());
            false
        }
        Err(_) => {
            tracing::warn!(
                timeout_ms = drain_timeout.as_millis() as u64,
                "gateway drain timed out; forcing shutdown"
            );
            server.join.abort();
            message = Some(format!(
                "drain timeout after {} ms; active requests were interrupted",
                drain_timeout.as_millis()
            ));
            true
        }
    };

    let _ = server.join.await;
    while exit_rx.try_recv().is_ok() {}

    let _ = events.send(ServiceEvent::Stopped {
        request_id,
        forced,
        message,
    });
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[tokio::test]
    async fn starts_stops_and_shuts_down() {
        let path = std::env::temp_dir().join(format!(
            "tomato-tui-supervisor-{}-{}.toml",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &path,
            r#"
            [server]
            listen = "127.0.0.1:0"

            [[providers]]
            id = "relay"
            base_url = "https://relay.example/v1"
            api_key = "sk-test"
            "#,
        )
        .unwrap();

        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(run(command_rx, event_tx, Duration::from_secs(2)));

        command_tx
            .send(Command::Start {
                request_id: 1,
                config_path: path.clone(),
                listen_override: None,
            })
            .unwrap();

        let mut started = false;
        for _ in 0..8 {
            match tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
                .await
                .unwrap()
            {
                Some(ServiceEvent::Running {
                    request_id: 1,
                    listen,
                    ..
                }) => {
                    assert_ne!(listen.port(), 0);
                    started = true;
                    break;
                }
                Some(ServiceEvent::Failed { message, .. }) => {
                    panic!("start failed: {message}")
                }
                Some(_) => {}
                None => panic!("supervisor event channel closed"),
            }
        }
        assert!(started, "supervisor did not report a running gateway");

        command_tx.send(Command::Stop { request_id: 2 }).unwrap();
        let mut stopped = false;
        for _ in 0..8 {
            match tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
                .await
                .unwrap()
            {
                Some(ServiceEvent::Stopped {
                    request_id: 2,
                    forced,
                    ..
                }) => {
                    assert!(!forced);
                    stopped = true;
                    break;
                }
                Some(ServiceEvent::Failed { message, .. }) => {
                    panic!("stop failed: {message}")
                }
                Some(_) => {}
                None => panic!("supervisor event channel closed"),
            }
        }
        assert!(stopped, "supervisor did not report a stopped gateway");

        command_tx.send(Command::Shutdown).unwrap();
        drop(command_tx);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();

        let _ = fs::remove_file(&path);
    }
}
