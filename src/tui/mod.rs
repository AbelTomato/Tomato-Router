//! Interactive terminal console for configuration and service control.

pub mod app;
pub mod draft;
pub mod input;
pub mod logbuf;
pub mod supervisor;
pub mod ui;

use std::io::{IsTerminal, stdout};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEvent, KeyEventKind,
};
use crossterm::execute;
use tokio::sync::mpsc;

use app::{App, NoticeKind, UiInit};
use draft::LoadedDraft;
use logbuf::LogBuffer;
use supervisor::Command;

/// Options collected from the CLI before the terminal is initialized.
pub struct Options {
    pub config_path: PathBuf,
    pub listen_override: Option<String>,
    pub log_filter: Option<String>,
    pub autostart: bool,
}

enum UiEvent {
    Key(KeyEvent),
    Paste(String),
    Resize,
}

/// Run the TUI until the user quits.
///
/// The gateway is supervised in a Tokio task inside this process. When the
/// TUI exits, the supervisor drains active requests (up to five seconds) and
/// stops the server.
pub async fn run(options: Options) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("`tui` requires an interactive terminal");
    }

    let log_buffer = LogBuffer::new(4_000);
    logbuf::init_tracing(options.log_filter.as_deref(), log_buffer.clone())?;

    let loaded = LoadedDraft::load(&options.config_path);
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let supervisor = tokio::spawn(supervisor::run(
        command_rx,
        event_tx,
        Duration::from_secs(5),
    ));

    let mut terminal = TerminalSession::enter()?;
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let input = spawn_input_thread(ui_tx)?;

    let mut app = App::new(UiInit {
        loaded,
        listen_override: options.listen_override,
        command_tx: command_tx.clone(),
        event_rx,
        log_buffer,
        autostart: options.autostart,
    });

    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut outcome = Ok(());
    loop {
        app.poll_events();
        app.on_tick();
        if let Err(error) = terminal.draw(&mut app) {
            outcome = Err(error).context("failed to draw the TUI");
            break;
        }
        if app.should_exit {
            break;
        }

        tokio::select! {
            _ = ticker.tick() => {}
            event = ui_rx.recv() => {
                match event {
                    Some(UiEvent::Key(key)) => {
                        if key.kind != KeyEventKind::Release {
                            app.on_key(key);
                        }
                    }
                    Some(UiEvent::Paste(text)) => app.on_paste(&text),
                    Some(UiEvent::Resize) => {}
                    None => {
                        app.set_notice(NoticeKind::Error, "terminal input closed");
                        break;
                    }
                }
            }
        }
    }

    let _ = command_tx.send(Command::Shutdown);
    drop(app);
    drop(ui_rx);
    let _ = input.join();
    drop(terminal);

    match tokio::time::timeout(Duration::from_secs(10), supervisor).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            outcome = outcome.and(Err(error).context("TUI supervisor panicked"));
        }
        Err(_) => {
            tracing::warn!("TUI supervisor did not stop within 10 seconds");
        }
    }

    outcome
}

fn spawn_input_thread(tx: mpsc::UnboundedSender<UiEvent>) -> Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("tomato-tui-input".to_string())
        .spawn(move || {
            loop {
                match event::poll(Duration::from_millis(100)) {
                    Ok(true) => match event::read() {
                        Ok(Event::Key(key)) => {
                            if tx.send(UiEvent::Key(key)).is_err() {
                                break;
                            }
                        }
                        Ok(Event::Paste(text)) => {
                            if tx.send(UiEvent::Paste(text)).is_err() {
                                break;
                            }
                        }
                        Ok(Event::Resize(_, _)) => {
                            if tx.send(UiEvent::Resize).is_err() {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    },
                    Ok(false) => {
                        if tx.is_closed() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .context("failed to start the terminal input thread")
}

struct TerminalSession {
    terminal: ratatui::DefaultTerminal,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        let terminal = ratatui::try_init().context("failed to initialize the terminal")?;
        if let Err(error) = execute!(stdout(), EnableBracketedPaste) {
            ratatui::restore();
            return Err(error).context("failed to enable bracketed paste");
        }
        Ok(Self { terminal })
    }

    fn draw(&mut self, app: &mut App) -> std::io::Result<()> {
        self.terminal.draw(|frame| ui::draw(frame, app))?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(stdout(), DisableBracketedPaste);
        ratatui::restore();
    }
}
