//! TUI application state, key handling and form editing.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;
use tokio::sync::mpsc;
use url::Url;

use crate::config::{
    AuthConfig, AuthKind, CompatConfig, Config, ProviderConfig, RouteConfig, ServerConfig, WireApi,
};
use crate::state::AppState;

use super::draft::{
    Draft, LoadedDraft, Report, create_from_example, format_comma_list, format_model_map,
    parse_comma_list, parse_model_map,
};
use super::input::{EditOutcome, TextEdit};
use super::logbuf::{LogBuffer, LogLevel, LogLine};
use super::supervisor::{Command, ServiceEvent};
use crate::provider::{BreakerStatus, ProviderSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Dashboard,
    Providers,
    Routes,
    Settings,
    Logs,
    Help,
}

impl Screen {
    pub const ALL: [Screen; 6] = [
        Screen::Dashboard,
        Screen::Providers,
        Screen::Routes,
        Screen::Settings,
        Screen::Logs,
        Screen::Help,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Dashboard => "Dashboard",
            Self::Providers => "Providers",
            Self::Routes => "Routes",
            Self::Settings => "Settings",
            Self::Logs => "Logs",
            Self::Help => "Help",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|screen| *screen == self)
            .unwrap_or(0)
    }

    fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    fn previous(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServicePhase {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

impl ServicePhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stopped => "STOPPED",
            Self::Starting => "STARTING",
            Self::Running => "RUNNING",
            Self::Stopping => "STOPPING",
            Self::Failed => "FAILED",
        }
    }
}

pub struct ServiceView {
    pub phase: ServicePhase,
    pub state: Option<AppState>,
    pub listen: Option<std::net::SocketAddr>,
    pub active_requests: usize,
    pub last_error: Option<String>,
    pub request_id: u64,
    pub quit_requested: bool,
    pub quit_deadline: Option<Instant>,
}

impl Default for ServiceView {
    fn default() -> Self {
        Self {
            phase: ServicePhase::Stopped,
            state: None,
            listen: None,
            active_requests: 0,
            last_error: None,
            request_id: 0,
            quit_requested: false,
            quit_deadline: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Success,
    Warning,
    Error,
}

pub struct Notice {
    pub kind: NoticeKind,
    pub text: String,
}

pub struct App {
    pub config_path: PathBuf,
    pub listen_override: Option<String>,
    pub draft: Option<Draft>,
    pub load_error: Option<String>,
    pub missing_config: bool,
    pub screen: Screen,
    pub modal: Option<Modal>,
    pub provider_selection: usize,
    pub route_selection: usize,
    pub help_scroll: u16,
    pub log_level: LogLevel,
    pub log_follow: bool,
    pub log_back: usize,
    pub log_unseen: u64,
    log_seen_version: u64,
    pub log_buffer: LogBuffer,
    pub command_tx: mpsc::UnboundedSender<Command>,
    pub event_rx: mpsc::UnboundedReceiver<ServiceEvent>,
    pub service: ServiceView,
    pub notice: Option<Notice>,
    pub should_exit: bool,
    pub next_request_id: u64,
    last_sample: Instant,
    last_requests_total: u64,
    pub request_samples: VecDeque<u64>,
    pub provider_table: TableState,
    pub route_table: TableState,
}

pub struct UiInit {
    pub loaded: LoadedDraft,
    pub listen_override: Option<String>,
    pub command_tx: mpsc::UnboundedSender<Command>,
    pub event_rx: mpsc::UnboundedReceiver<ServiceEvent>,
    pub log_buffer: LogBuffer,
    pub autostart: bool,
}

impl App {
    pub fn new(init: UiInit) -> Self {
        let mut app = Self {
            config_path: init.loaded.path,
            listen_override: init.listen_override,
            draft: init.loaded.draft,
            load_error: init.loaded.error,
            missing_config: init.loaded.missing,
            screen: Screen::Dashboard,
            modal: None,
            provider_selection: 0,
            route_selection: 0,
            help_scroll: 0,
            log_level: LogLevel::Info,
            log_follow: true,
            log_back: 0,
            log_unseen: 0,
            log_seen_version: init.log_buffer.version(),
            log_buffer: init.log_buffer,
            command_tx: init.command_tx,
            event_rx: init.event_rx,
            service: ServiceView::default(),
            notice: None,
            should_exit: false,
            next_request_id: 1,
            last_sample: Instant::now(),
            last_requests_total: 0,
            request_samples: VecDeque::new(),
            provider_table: TableState::default(),
            route_table: TableState::default(),
        };
        if let Some(error) = &app.load_error {
            app.set_notice(NoticeKind::Error, error.clone());
        }
        if init.autostart {
            app.request_start();
        }
        app
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.request_quit();
            return;
        }
        if key.code == KeyCode::Char('s')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.modal.is_none()
        {
            self.save_draft();
        }

        if self.modal.is_some() {
            self.handle_modal_key(key);
            return;
        }

        match key.code {
            KeyCode::Char('1') => self.set_screen(Screen::Dashboard),
            KeyCode::Char('2') => self.set_screen(Screen::Providers),
            KeyCode::Char('3') => self.set_screen(Screen::Routes),
            KeyCode::Char('4') => self.set_screen(Screen::Settings),
            KeyCode::Char('5') => self.set_screen(Screen::Logs),
            KeyCode::Char('6') => self.set_screen(Screen::Help),
            KeyCode::Tab => self.set_screen(self.screen.next()),
            KeyCode::BackTab => self.set_screen(self.screen.previous()),
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('?') => {
                if self.screen == Screen::Help {
                    self.set_screen(Screen::Dashboard);
                } else {
                    self.set_screen(Screen::Help);
                }
            }
            KeyCode::Char('s') => self.request_start(),
            KeyCode::Char('x') => self.request_stop(),
            KeyCode::Char('r') => self.request_restart(),
            KeyCode::Char('w') => self.save_draft(),
            KeyCode::Char('v') => self.show_validation(),
            KeyCode::Char('R') => self.request_reload(),
            KeyCode::F(5) => self.request_apply(),
            KeyCode::Esc => {
                if self.screen != Screen::Dashboard {
                    self.set_screen(Screen::Dashboard);
                }
            }
            KeyCode::Char('c') if self.screen != Screen::Logs && self.missing_config => {
                self.create_config()
            }
            _ => self.handle_screen_key(key),
        }
    }

    fn handle_screen_key(&mut self, key: KeyEvent) {
        match self.screen {
            Screen::Providers => self.handle_provider_key(key),
            Screen::Routes => self.handle_route_key(key),
            Screen::Settings => {
                if key.code == KeyCode::Enter {
                    self.open_server_form();
                }
            }
            Screen::Logs => self.handle_log_key(key),
            Screen::Help => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1)
                }
                KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(10),
                KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(10),
                KeyCode::Home => self.help_scroll = 0,
                KeyCode::End => self.help_scroll = u16::MAX,
                _ => {}
            },
            Screen::Dashboard => {}
        }
    }

    fn handle_provider_key(&mut self, key: KeyEvent) {
        let Some(draft) = &self.draft else {
            return;
        };
        let len = draft.config.providers.len();
        match key.code {
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_provider(self.provider_selection, -1)
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_provider(self.provider_selection, 1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if len > 0 {
                    self.provider_selection = self.provider_selection.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if len > 0 {
                    self.provider_selection = (self.provider_selection + 1).min(len - 1);
                }
            }
            KeyCode::Enter => {
                if self.provider_selection < len {
                    self.open_provider_form(Some(self.provider_selection));
                }
            }
            KeyCode::Char('a') => self.open_provider_form(None),
            KeyCode::Char('d') => self.request_delete_provider(self.provider_selection),
            _ => {}
        }
    }

    fn handle_route_key(&mut self, key: KeyEvent) {
        let Some(draft) = &self.draft else {
            return;
        };
        let len = draft.config.routes.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if len > 0 {
                    self.route_selection = self.route_selection.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if len > 0 {
                    self.route_selection = (self.route_selection + 1).min(len - 1);
                }
            }
            KeyCode::Enter => {
                if self.route_selection < len {
                    self.open_route_form(Some(self.route_selection));
                }
            }
            KeyCode::Char('a') => self.open_route_form(None),
            KeyCode::Char('d') => self.request_delete_route(self.route_selection),
            _ => {}
        }
    }

    fn handle_log_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.log_follow = false;
                let total = self.visible_log_count();
                self.log_back = (self.log_back + 1).min(total.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.log_back > 0 {
                    self.log_back -= 1;
                    if self.log_back == 0 {
                        self.log_follow = true;
                    }
                }
            }
            KeyCode::PageUp => {
                self.log_follow = false;
                let total = self.visible_log_count();
                self.log_back = (self.log_back + 10).min(total.saturating_sub(1));
            }
            KeyCode::PageDown => {
                self.log_back = self.log_back.saturating_sub(10);
                if self.log_back == 0 {
                    self.log_follow = true;
                }
            }
            KeyCode::Home => {
                self.log_follow = false;
                self.log_back = self.visible_log_count().saturating_sub(1);
            }
            KeyCode::End => {
                self.log_follow = true;
                self.log_back = 0;
            }
            KeyCode::Char('f') => {
                self.log_follow = !self.log_follow;
                if self.log_follow {
                    self.log_back = 0;
                    self.log_unseen = 0;
                }
            }
            KeyCode::Char('l') => self.cycle_log_level(),
            KeyCode::Char('c') => self.clear_logs(),
            _ => {}
        }
    }

    fn set_screen(&mut self, screen: Screen) {
        self.screen = screen;
        if screen == Screen::Logs {
            self.log_unseen = 0;
            self.log_seen_version = self.log_buffer.version();
        }
        self.clamp_selections();
    }

    pub fn on_paste(&mut self, text: &str) {
        if let Some(Modal::Form(form)) = &mut self.modal
            && let Some(edit) = &mut form.editing
        {
            edit.insert_str(text);
        }
    }

    pub fn on_tick(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_sample) >= Duration::from_secs(1) {
            self.last_sample = now;
            let total = self
                .service
                .state
                .as_ref()
                .map(|state| state.metrics.requests_total())
                .unwrap_or(0);
            let delta = total.saturating_sub(self.last_requests_total);
            self.last_requests_total = total;
            self.request_samples.push_back(delta);
            while self.request_samples.len() > 60 {
                self.request_samples.pop_front();
            }
        }

        let version = self.log_buffer.version();
        if version != self.log_seen_version {
            if !self.log_follow || self.screen != Screen::Logs {
                self.log_unseen = self
                    .log_unseen
                    .saturating_add(version.saturating_sub(self.log_seen_version));
            }
            self.log_seen_version = version;
        }

        if self.screen == Screen::Logs && self.log_follow {
            self.log_unseen = 0;
        }

        if self.service.quit_requested
            && let Some(deadline) = self.service.quit_deadline
            && now >= deadline
        {
            self.should_exit = true;
        }
    }

    pub fn poll_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            self.apply_event(event);
        }
    }

    fn apply_event(&mut self, event: ServiceEvent) {
        let request_id = match &event {
            ServiceEvent::Starting { request_id }
            | ServiceEvent::Running { request_id, .. }
            | ServiceEvent::Stopping { request_id, .. }
            | ServiceEvent::Stopped { request_id, .. }
            | ServiceEvent::Failed { request_id, .. } => *request_id,
        };
        if request_id != 0 && request_id < self.service.request_id {
            return;
        }

        match event {
            ServiceEvent::Starting { request_id } => {
                self.service.phase = ServicePhase::Starting;
                self.service.request_id = request_id.max(self.service.request_id);
                self.service.last_error = None;
            }
            ServiceEvent::Running {
                request_id,
                state,
                listen,
            } => {
                self.service.phase = ServicePhase::Running;
                self.service.request_id = request_id.max(self.service.request_id);
                self.service.state = Some(state);
                self.service.listen = Some(listen);
                self.service.last_error = None;
                self.service.active_requests = 0;
                self.last_requests_total = 0;
                self.request_samples.clear();
                self.set_notice(NoticeKind::Success, format!("Gateway running on {listen}"));
            }
            ServiceEvent::Stopping {
                request_id,
                active_requests,
            } => {
                self.service.phase = ServicePhase::Stopping;
                self.service.request_id = request_id.max(self.service.request_id);
                self.service.active_requests = active_requests;
            }
            ServiceEvent::Stopped {
                request_id,
                forced,
                message,
            } => {
                self.service.phase = ServicePhase::Stopped;
                self.service.request_id = request_id.max(self.service.request_id);
                self.service.state = None;
                self.service.listen = None;
                self.service.active_requests = 0;
                let text = message.unwrap_or_else(|| {
                    if forced {
                        "Gateway stopped after the drain timeout".to_string()
                    } else {
                        "Gateway stopped".to_string()
                    }
                });
                let kind = if forced {
                    NoticeKind::Warning
                } else {
                    NoticeKind::Info
                };
                self.set_notice(kind, text);
                if self.service.quit_requested {
                    self.should_exit = true;
                }
            }
            ServiceEvent::Failed {
                request_id,
                message,
            } => {
                self.service.phase = ServicePhase::Failed;
                self.service.request_id = request_id.max(self.service.request_id);
                self.service.state = None;
                self.service.listen = None;
                self.service.last_error = Some(message.clone());
                self.set_notice(NoticeKind::Error, message);
                if self.service.quit_requested {
                    self.should_exit = true;
                }
            }
        }
    }

    pub fn set_notice(&mut self, kind: NoticeKind, text: impl Into<String>) {
        self.notice = Some(Notice {
            kind,
            text: text.into(),
        });
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        id
    }

    fn send(&mut self, command: Command) -> bool {
        if self.command_tx.send(command).is_ok() {
            true
        } else {
            self.set_notice(NoticeKind::Error, "TUI supervisor is not available");
            false
        }
    }

    pub fn request_start(&mut self) {
        match self.service.phase {
            ServicePhase::Running => {
                self.set_notice(NoticeKind::Info, "Gateway is already running");
                return;
            }
            ServicePhase::Starting | ServicePhase::Stopping => {
                self.set_notice(NoticeKind::Info, "Gateway operation is already in progress");
                return;
            }
            _ => {}
        }
        if self.draft.is_none() {
            self.show_info(
                "Gateway not available",
                vec![
                    self.load_error
                        .clone()
                        .unwrap_or_else(|| "No configuration is loaded".to_string()),
                ],
            );
            return;
        }
        if self.draft.as_ref().is_some_and(Draft::is_dirty) {
            self.open_confirm(
                "Save before starting?",
                "The configuration has unsaved changes. Save it before starting the gateway.",
                ConfirmAction::SaveAndStart,
            );
            return;
        }
        self.begin_start();
    }

    fn begin_start(&mut self) {
        let Some(draft) = &self.draft else {
            return;
        };
        let report = draft.report();
        if !report.can_start {
            self.show_report("Cannot start gateway", &report);
            return;
        }
        let id = self.next_request_id();
        self.service.request_id = id;
        self.service.phase = ServicePhase::Starting;
        self.service.last_error = None;
        let command = Command::Start {
            request_id: id,
            config_path: self.config_path.clone(),
            listen_override: self.listen_override.clone(),
        };
        if self.send(command) {
            self.set_notice(NoticeKind::Info, "Starting gateway...");
        }
    }

    pub fn request_stop(&mut self) {
        match self.service.phase {
            ServicePhase::Stopped | ServicePhase::Failed => {
                self.set_notice(NoticeKind::Info, "Gateway is not running");
                return;
            }
            ServicePhase::Stopping => {
                self.set_notice(NoticeKind::Info, "Gateway is already stopping");
                return;
            }
            _ => {}
        }
        let active = self.active_request_count();
        if active > 0 {
            self.open_confirm(
                "Stop gateway?",
                format!(
                    "{active} request(s) are still active. They will be interrupted after the 5 second drain timeout."
                ),
                ConfirmAction::Stop,
            );
        } else {
            self.send_stop();
        }
    }

    fn send_stop(&mut self) {
        let id = self.next_request_id();
        self.service.request_id = id;
        self.service.phase = ServicePhase::Stopping;
        if self.send(Command::Stop { request_id: id }) {
            self.set_notice(NoticeKind::Info, "Stopping gateway...");
        }
    }

    pub fn request_restart(&mut self) {
        match self.service.phase {
            ServicePhase::Starting | ServicePhase::Stopping => {
                self.set_notice(NoticeKind::Info, "Gateway operation is already in progress");
                return;
            }
            _ => {}
        }
        if self.draft.is_none() {
            self.show_info(
                "Gateway not available",
                vec![
                    self.load_error
                        .clone()
                        .unwrap_or_else(|| "No configuration is loaded".to_string()),
                ],
            );
            return;
        }
        if self.draft.as_ref().is_some_and(Draft::is_dirty) {
            self.open_confirm(
                "Save before restarting?",
                "The configuration has unsaved changes. Save it before restarting the gateway.",
                ConfirmAction::SaveAndRestart,
            );
            return;
        }
        self.begin_restart();
    }

    fn begin_restart(&mut self) {
        let Some(draft) = &self.draft else {
            return;
        };
        let report = draft.report();
        if !report.can_start {
            self.show_report("Cannot restart gateway", &report);
            return;
        }
        let id = self.next_request_id();
        self.service.request_id = id;
        self.service.phase = ServicePhase::Starting;
        self.service.last_error = None;
        let command = Command::Restart {
            request_id: id,
            config_path: self.config_path.clone(),
            listen_override: self.listen_override.clone(),
        };
        if self.send(command) {
            self.set_notice(NoticeKind::Info, "Restarting gateway...");
        }
    }

    pub fn request_apply(&mut self) {
        let running = self.service.phase == ServicePhase::Running;
        if self.draft.as_ref().is_some_and(Draft::is_dirty) {
            self.open_confirm(
                "Save and apply?",
                "Save the configuration and restart the gateway so the changes take effect.",
                if running {
                    ConfirmAction::SaveAndRestart
                } else {
                    ConfirmAction::SaveAndStart
                },
            );
        } else if running {
            self.request_restart();
        } else {
            self.request_start();
        }
    }

    pub fn save_draft(&mut self) {
        if self.try_save() {
            self.set_notice(NoticeKind::Success, "Configuration saved");
            if self.pending_restart() {
                self.set_notice(
                    NoticeKind::Warning,
                    "Configuration saved; restart the gateway to apply it",
                );
            }
        }
    }

    fn try_save(&mut self) -> bool {
        let Some(draft) = &mut self.draft else {
            self.show_info(
                "Cannot save",
                vec![
                    self.load_error
                        .clone()
                        .unwrap_or_else(|| "No configuration is loaded".to_string()),
                ],
            );
            return false;
        };
        let report = draft.report();
        if !report.can_save {
            self.show_report("Cannot save configuration", &report);
            return false;
        }
        match draft.save() {
            Ok(outcome) => {
                if let Some(backup) = outcome.backup {
                    self.set_notice(
                        NoticeKind::Success,
                        format!("Configuration saved; backup: {}", backup.display()),
                    );
                }
                true
            }
            Err(error) => {
                self.show_info("Save failed", vec![format!("{error:#}")]);
                false
            }
        }
    }

    fn save_then(&mut self, action: PendingAction) {
        if !self.try_save() {
            return;
        }
        match action {
            PendingAction::Start => self.begin_start(),
            PendingAction::Restart => self.begin_restart(),
        }
    }

    pub fn request_reload(&mut self) {
        if self.draft.as_ref().is_some_and(Draft::is_dirty) {
            self.open_confirm(
                "Reload configuration?",
                "Discard unsaved changes and reload the configuration from disk.",
                ConfirmAction::Reload,
            );
        } else {
            self.reload_config();
        }
    }

    fn reload_config(&mut self) {
        let loaded = LoadedDraft::load(&self.config_path);
        self.missing_config = loaded.missing;
        self.load_error = loaded.error.clone();
        self.draft = loaded.draft;
        self.clamp_selections();
        match &self.load_error {
            Some(error) => self.set_notice(NoticeKind::Error, error.clone()),
            None => self.set_notice(NoticeKind::Success, "Configuration reloaded"),
        }
    }

    fn create_config(&mut self) {
        match create_from_example(&self.config_path) {
            Ok(()) => {
                self.set_notice(
                    NoticeKind::Success,
                    format!("Created {}", self.config_path.display()),
                );
                self.reload_config();
            }
            Err(error) => {
                self.show_info("Cannot create configuration", vec![format!("{error:#}")]);
            }
        }
    }

    pub fn request_quit(&mut self) {
        if matches!(
            self.service.phase,
            ServicePhase::Running | ServicePhase::Starting | ServicePhase::Stopping
        ) && !self.service.quit_requested
        {
            self.open_confirm(
                "Quit Tomato Router?",
                "The gateway runs inside this TUI. Quitting will stop the gateway.",
                ConfirmAction::Quit,
            );
        } else {
            self.begin_quit();
        }
    }

    fn begin_quit(&mut self) {
        self.service.quit_requested = true;
        self.service.quit_deadline = Some(Instant::now() + Duration::from_secs(8));
        let _ = self.command_tx.send(Command::Shutdown);
        if !matches!(
            self.service.phase,
            ServicePhase::Starting | ServicePhase::Running | ServicePhase::Stopping
        ) {
            self.should_exit = true;
        } else {
            self.set_notice(NoticeKind::Info, "Stopping gateway before exit...");
        }
    }

    fn open_provider_form(&mut self, index: Option<usize>) {
        let Some(draft) = &self.draft else {
            self.show_info(
                "Cannot edit providers",
                vec![
                    self.load_error
                        .clone()
                        .unwrap_or_else(|| "No configuration is loaded".to_string()),
                ],
            );
            return;
        };
        self.modal = Some(Modal::Form(Box::new(FormState::provider(
            index,
            &draft.config,
        ))));
    }

    fn open_route_form(&mut self, index: Option<usize>) {
        let Some(draft) = &self.draft else {
            self.show_info(
                "Cannot edit routes",
                vec![
                    self.load_error
                        .clone()
                        .unwrap_or_else(|| "No configuration is loaded".to_string()),
                ],
            );
            return;
        };
        self.modal = Some(Modal::Form(Box::new(FormState::route(
            index,
            &draft.config,
        ))));
    }

    fn open_server_form(&mut self) {
        let Some(draft) = &self.draft else {
            self.show_info(
                "Cannot edit settings",
                vec![
                    self.load_error
                        .clone()
                        .unwrap_or_else(|| "No configuration is loaded".to_string()),
                ],
            );
            return;
        };
        self.modal = Some(Modal::Form(Box::new(FormState::server(
            &draft.config.server,
        ))));
    }

    fn request_delete_provider(&mut self, index: usize) {
        let Some(draft) = &self.draft else {
            return;
        };
        let Some(provider) = draft.config.providers.get(index) else {
            return;
        };
        if draft.config.providers.len() <= 1 {
            self.show_info(
                "Cannot delete provider",
                vec!["At least one provider is required.".to_string()],
            );
            return;
        }
        let routes: Vec<String> = draft
            .config
            .routes
            .iter()
            .filter(|route| route.chain.iter().any(|id| id == &provider.id))
            .map(|route| route.model.clone())
            .collect();
        if !routes.is_empty() {
            self.show_info(
                "Cannot delete provider",
                vec![format!(
                    "Provider `{}` is used by route(s): {}",
                    provider.id,
                    routes.join(", ")
                )],
            );
            return;
        }
        self.open_confirm(
            "Delete provider?",
            format!("Delete provider `{}`?", provider.id),
            ConfirmAction::DeleteProvider(index),
        );
    }

    fn request_delete_route(&mut self, index: usize) {
        let Some(draft) = &self.draft else {
            return;
        };
        let Some(route) = draft.config.routes.get(index) else {
            return;
        };
        self.open_confirm(
            "Delete route?",
            format!("Delete route `{}`?", route.model),
            ConfirmAction::DeleteRoute(index),
        );
    }

    fn move_provider(&mut self, index: usize, delta: isize) {
        let Some(draft) = &mut self.draft else {
            return;
        };
        let len = draft.config.providers.len();
        if len == 0 {
            return;
        }
        let target = if delta < 0 {
            index.saturating_sub(1)
        } else {
            (index + 1).min(len - 1)
        };
        if target == index {
            return;
        }
        draft.config.providers.swap(index, target);
        self.provider_selection = target;
    }

    fn open_confirm(&mut self, title: &str, message: impl Into<String>, action: ConfirmAction) {
        self.modal = Some(Modal::Confirm(Box::new(ConfirmState {
            title: title.to_string(),
            message: message.into(),
            action,
        })));
    }

    pub fn show_info(&mut self, title: &str, lines: Vec<String>) {
        self.modal = Some(Modal::Info(InfoState {
            title: title.to_string(),
            lines,
            scroll: 0,
        }));
    }

    fn show_report(&mut self, title: &str, report: &Report) {
        let mut lines = report.summary.clone();
        if !report.errors.is_empty() {
            lines.push(String::new());
            lines.push("Errors:".to_string());
            lines.extend(report.errors.iter().map(|error| format!("  - {error}")));
        }
        if !report.warnings.is_empty() {
            lines.push(String::new());
            lines.push("Warnings:".to_string());
            lines.extend(
                report
                    .warnings
                    .iter()
                    .map(|warning| format!("  - {warning}")),
            );
        }
        lines.push(String::new());
        lines.push(format!(
            "Save allowed: {}    Start allowed: {}",
            if report.can_save { "yes" } else { "no" },
            if report.can_start { "yes" } else { "no" }
        ));
        self.show_info(title, lines);
    }

    pub fn show_validation(&mut self) {
        let Some(draft) = &self.draft else {
            self.show_info(
                "Validation",
                vec![
                    self.load_error
                        .clone()
                        .unwrap_or_else(|| "No configuration is loaded".to_string()),
                ],
            );
            return;
        };
        let report = draft.report();
        self.show_report("Configuration validation", &report);
    }

    fn handle_modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.take() else {
            return;
        };
        match modal {
            Modal::Form(mut form) => match form.handle_key(key) {
                FormAction::Continue => {
                    self.modal = Some(Modal::Form(form));
                }
                FormAction::Cancel => {}
                FormAction::Apply => {
                    let kind = form.kind;
                    let Some(draft) = &mut self.draft else {
                        form.error = Some("No configuration is loaded".to_string());
                        self.modal = Some(Modal::Form(form));
                        return;
                    };
                    match form.apply(&mut draft.config) {
                        Ok(()) => {
                            self.after_form_applied(kind);
                            self.set_notice(NoticeKind::Success, "Configuration updated");
                        }
                        Err(error) => {
                            form.error = Some(error);
                            self.modal = Some(Modal::Form(form));
                        }
                    }
                }
            },
            Modal::Confirm(confirm) => {
                if is_confirm_key(key) {
                    self.execute_confirm(confirm.action);
                } else if !matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    self.modal = Some(Modal::Confirm(confirm));
                }
            }
            Modal::Info(mut info) => {
                if info.handle_key(key) {
                    self.modal = Some(Modal::Info(info));
                }
            }
        }
    }

    fn execute_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::DeleteProvider(index) => {
                if let Some(draft) = &mut self.draft
                    && index < draft.config.providers.len()
                {
                    let removed = draft.config.providers.remove(index);
                    self.set_notice(
                        NoticeKind::Success,
                        format!("Deleted provider `{}`", removed.id),
                    );
                }
                self.clamp_selections();
            }
            ConfirmAction::DeleteRoute(index) => {
                if let Some(draft) = &mut self.draft
                    && index < draft.config.routes.len()
                {
                    let removed = draft.config.routes.remove(index);
                    self.set_notice(
                        NoticeKind::Success,
                        format!("Deleted route `{}`", removed.model),
                    );
                }
                self.clamp_selections();
            }
            ConfirmAction::Stop => self.send_stop(),
            ConfirmAction::Quit => self.begin_quit(),
            ConfirmAction::SaveAndStart => self.save_then(PendingAction::Start),
            ConfirmAction::SaveAndRestart => self.save_then(PendingAction::Restart),
            ConfirmAction::Reload => self.reload_config(),
        }
    }

    fn after_form_applied(&mut self, kind: FormKind) {
        match kind {
            FormKind::Provider { index: Some(index) } => {
                self.provider_selection = index;
                self.screen = Screen::Providers;
            }
            FormKind::Provider { index: None } => {
                self.screen = Screen::Providers;
                self.provider_selection = self
                    .draft
                    .as_ref()
                    .map(|draft| draft.config.providers.len().saturating_sub(1))
                    .unwrap_or(0);
            }
            FormKind::Route { index: Some(index) } => {
                self.route_selection = index;
                self.screen = Screen::Routes;
            }
            FormKind::Route { index: None } => {
                self.screen = Screen::Routes;
                self.route_selection = self
                    .draft
                    .as_ref()
                    .map(|draft| draft.config.routes.len().saturating_sub(1))
                    .unwrap_or(0);
            }
            FormKind::Server => {
                self.screen = Screen::Settings;
            }
        }
        self.clamp_selections();
    }

    fn clamp_selections(&mut self) {
        let providers = self
            .draft
            .as_ref()
            .map(|draft| draft.config.providers.len())
            .unwrap_or(0);
        let routes = self
            .draft
            .as_ref()
            .map(|draft| draft.config.routes.len())
            .unwrap_or(0);
        if providers == 0 {
            self.provider_selection = 0;
        } else {
            self.provider_selection = self.provider_selection.min(providers - 1);
        }
        if routes == 0 {
            self.route_selection = 0;
        } else {
            self.route_selection = self.route_selection.min(routes - 1);
        }
    }

    fn cycle_log_level(&mut self) {
        self.log_level = match self.log_level {
            LogLevel::Info => LogLevel::Warn,
            LogLevel::Warn => LogLevel::Error,
            LogLevel::Error => LogLevel::Debug,
            LogLevel::Debug => LogLevel::Trace,
            LogLevel::Trace => LogLevel::Info,
        };
        self.log_follow = true;
        self.log_back = 0;
        self.log_unseen = 0;
    }

    fn clear_logs(&mut self) {
        self.log_buffer.clear();
        self.log_back = 0;
        self.log_follow = true;
        self.log_unseen = 0;
        self.log_seen_version = self.log_buffer.version();
    }

    fn visible_log_count(&self) -> usize {
        self.log_buffer
            .snapshot()
            .into_iter()
            .filter(|line| line.level >= self.log_level)
            .count()
    }

    pub fn active_request_count(&self) -> usize {
        self.service
            .state
            .as_ref()
            .map(|state| {
                state
                    .providers
                    .all()
                    .iter()
                    .map(|provider| provider.inflight() + provider.queue_depth())
                    .sum()
            })
            .unwrap_or(0)
    }

    pub fn active_config(&self) -> Option<&Config> {
        self.draft.as_ref().map(|draft| &draft.config).or_else(|| {
            self.service
                .state
                .as_ref()
                .map(|state| state.config.as_ref())
        })
    }

    pub fn pending_restart(&self) -> bool {
        match (&self.draft, &self.service.state) {
            (Some(draft), Some(state)) => {
                if self.listen_override.is_some() {
                    let mut expected = draft.config.clone();
                    if let Some(listen) = &self.listen_override {
                        expected.server.listen = listen.clone();
                    }
                    expected != *state.config
                } else {
                    draft.config != *state.config
                }
            }
            _ => false,
        }
    }

    pub fn filtered_logs(&self) -> Vec<LogLine> {
        self.log_buffer
            .snapshot()
            .into_iter()
            .filter(|line| line.level >= self.log_level)
            .collect()
    }

    pub fn provider_snapshots(&self) -> Vec<ProviderSnapshot> {
        self.service
            .state
            .as_ref()
            .map(|state| {
                state
                    .providers
                    .all()
                    .iter()
                    .map(|provider| provider.snapshot())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn request_total_last_minute(&self) -> u64 {
        self.request_samples.iter().sum()
    }
}

pub enum Modal {
    Confirm(Box<ConfirmState>),
    Form(Box<FormState>),
    Info(InfoState),
}

pub struct ConfirmState {
    pub title: String,
    pub message: String,
    pub action: ConfirmAction,
}

#[derive(Debug, Clone, Copy)]
pub enum ConfirmAction {
    DeleteProvider(usize),
    DeleteRoute(usize),
    Stop,
    Quit,
    SaveAndStart,
    SaveAndRestart,
    Reload,
}

fn is_confirm_key(key: KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y')
    )
}

pub struct InfoState {
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: u16,
}

impl InfoState {
    /// Returns true when the modal should stay open.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1);
                true
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                true
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10);
                true
            }
            KeyCode::Home => {
                self.scroll = 0;
                true
            }
            KeyCode::End => {
                self.scroll = u16::MAX;
                true
            }
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => false,
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum FormKind {
    Provider { index: Option<usize> },
    Route { index: Option<usize> },
    Server,
}

#[derive(Clone)]
pub enum FormOriginal {
    Provider(Box<ProviderConfig>),
    Route(RouteConfig),
    Server(ServerConfig),
}

pub struct FormField {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
    pub value: String,
    pub boolean: bool,
    pub choice: usize,
    pub hint: &'static str,
    pub read_only: bool,
}

pub enum FieldKind {
    Text,
    Secret,
    SecretList,
    Number,
    Bool,
    Choice(&'static [&'static str]),
}

impl FormField {
    fn text(key: &'static str, label: &'static str, value: impl Into<String>) -> Self {
        Self {
            key,
            label,
            kind: FieldKind::Text,
            value: value.into(),
            boolean: false,
            choice: 0,
            hint: "",
            read_only: false,
        }
    }

    fn text_with_hint(
        key: &'static str,
        label: &'static str,
        value: impl Into<String>,
        hint: &'static str,
    ) -> Self {
        Self {
            hint,
            ..Self::text(key, label, value)
        }
    }

    fn secret(key: &'static str, label: &'static str, value: impl Into<String>) -> Self {
        Self {
            kind: FieldKind::Secret,
            ..Self::text(key, label, value)
        }
    }

    fn secret_list(key: &'static str, label: &'static str, value: impl Into<String>) -> Self {
        Self {
            kind: FieldKind::SecretList,
            ..Self::text(key, label, value)
        }
    }

    fn number(key: &'static str, label: &'static str, value: impl Into<String>) -> Self {
        Self {
            kind: FieldKind::Number,
            ..Self::text(key, label, value)
        }
    }

    fn boolean(key: &'static str, label: &'static str, value: bool) -> Self {
        Self {
            key,
            label,
            kind: FieldKind::Bool,
            value: String::new(),
            boolean: value,
            choice: 0,
            hint: "",
            read_only: false,
        }
    }

    fn choice(
        key: &'static str,
        label: &'static str,
        options: &'static [&'static str],
        current: &str,
    ) -> Self {
        let choice = options
            .iter()
            .position(|option| *option == current)
            .unwrap_or(0);
        Self {
            key,
            label,
            kind: FieldKind::Choice(options),
            value: String::new(),
            boolean: false,
            choice,
            hint: "",
            read_only: false,
        }
    }

    fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    pub fn display_value(&self) -> String {
        match &self.kind {
            FieldKind::Text | FieldKind::Number => {
                if self.value.is_empty() {
                    "(empty)".to_string()
                } else {
                    self.value.clone()
                }
            }
            FieldKind::Secret => {
                if self.value.is_empty() {
                    "(empty)".to_string()
                } else if self.value.contains("${") {
                    self.value.clone()
                } else {
                    "••••••".to_string()
                }
            }
            FieldKind::SecretList => {
                let items = parse_comma_list(&self.value);
                if items.is_empty() {
                    "(empty)".to_string()
                } else {
                    items
                        .iter()
                        .map(|item| {
                            if item.contains("${") {
                                item.clone()
                            } else {
                                "••••".to_string()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            }
            FieldKind::Bool => {
                if self.boolean {
                    "yes".to_string()
                } else {
                    "no".to_string()
                }
            }
            FieldKind::Choice(options) => options
                .get(self.choice)
                .copied()
                .unwrap_or("(unknown)")
                .to_string(),
        }
    }
}

pub struct FormState {
    pub kind: FormKind,
    pub title: String,
    pub fields: Vec<FormField>,
    pub selected: usize,
    pub editing: Option<TextEdit>,
    pub error: Option<String>,
    pub original: FormOriginal,
}

impl FormState {
    pub fn provider(index: Option<usize>, config: &Config) -> Self {
        let existing = index.and_then(|index| config.providers.get(index));
        let effective_index = existing.and(index);
        let provider = existing.cloned().unwrap_or_else(default_provider);
        let is_new = effective_index.is_none();
        let auth_kind = match provider.auth.as_ref().map(|auth| auth.kind) {
            None => "default",
            Some(AuthKind::Bearer) => "bearer",
            Some(AuthKind::Header) => "header",
            Some(AuthKind::Query) => "query",
            Some(AuthKind::None) => "none",
        };
        let auth = provider.auth.as_ref();

        let fields = vec![
            if is_new {
                FormField::text("id", "id", provider.id.clone())
            } else {
                FormField::text("id", "id", provider.id.clone()).read_only()
            },
            FormField::text("name", "name", provider.name.clone().unwrap_or_default()),
            FormField::text_with_hint(
                "base_url",
                "base_url",
                provider.base_url.clone(),
                "API root, usually ending in /v1",
            ),
            FormField::choice(
                "wire_api",
                "wire_api",
                &["chat", "responses", "messages"],
                &provider.wire_api.to_string(),
            ),
            FormField::text_with_hint(
                "api_key_env",
                "api_key_env",
                provider.api_key_env.clone().unwrap_or_default(),
                "Environment variable read when the gateway starts",
            ),
            FormField::secret(
                "api_key",
                "api_key",
                provider.api_key.clone().unwrap_or_default(),
            ),
            FormField::choice(
                "auth.kind",
                "auth.type",
                &["default", "bearer", "header", "query", "none"],
                auth_kind,
            ),
            FormField::text(
                "auth.header",
                "auth.header",
                auth.and_then(|auth| auth.header.clone())
                    .unwrap_or_default(),
            ),
            FormField::text(
                "auth.prefix",
                "auth.prefix",
                auth.and_then(|auth| auth.prefix.clone())
                    .unwrap_or_default(),
            ),
            FormField::text(
                "auth.query_param",
                "auth.query_param",
                auth.and_then(|auth| auth.query_param.clone())
                    .unwrap_or_default(),
            ),
            FormField::number(
                "max_concurrency",
                "max_concurrency",
                provider.max_concurrency.to_string(),
            ),
            FormField::number("max_queue", "max_queue", provider.max_queue.to_string()),
            FormField::number(
                "queue_timeout_ms",
                "queue_timeout_ms",
                provider.queue_timeout_ms.to_string(),
            ),
            FormField::number(
                "cooldown_ms",
                "cooldown_ms",
                provider.cooldown_ms.to_string(),
            ),
            FormField::number(
                "failure_threshold",
                "failure_threshold",
                provider.failure_threshold.to_string(),
            ),
            FormField::text_with_hint(
                "models",
                "models",
                format_model_map(&provider.models),
                "client=upstream, comma separated",
            ),
        ];

        Self {
            kind: FormKind::Provider {
                index: effective_index,
            },
            title: if is_new {
                "Add provider".to_string()
            } else {
                format!("Edit provider: {}", provider.id)
            },
            fields,
            selected: 0,
            editing: None,
            error: None,
            original: FormOriginal::Provider(Box::new(provider)),
        }
    }

    pub fn route(index: Option<usize>, config: &Config) -> Self {
        let existing = index.and_then(|index| config.routes.get(index));
        let effective_index = existing.and(index);
        let route = existing.cloned().unwrap_or_else(|| RouteConfig {
            model: String::new(),
            chain: config
                .providers
                .first()
                .map(|provider| vec![provider.id.clone()])
                .unwrap_or_default(),
        });
        let is_new = effective_index.is_none();
        let fields = vec![
            FormField::text_with_hint("model", "model", route.model.clone(), "Use * for fallback"),
            FormField::text_with_hint(
                "chain",
                "chain",
                format_comma_list(&route.chain),
                "provider ids in strict priority order",
            ),
        ];
        Self {
            kind: FormKind::Route {
                index: effective_index,
            },
            title: if is_new {
                "Add route".to_string()
            } else {
                format!("Edit route: {}", route.model)
            },
            fields,
            selected: 0,
            editing: None,
            error: None,
            original: FormOriginal::Route(route),
        }
    }

    pub fn server(server: &ServerConfig) -> Self {
        let fields = vec![
            FormField::text_with_hint(
                "listen",
                "listen",
                server.listen.clone(),
                "host:port; 0.0.0.0 requires local auth",
            ),
            FormField::boolean("require_auth", "require_auth", server.require_auth),
            FormField::secret_list("api_keys", "api_keys", server.api_keys.join(", ")),
            FormField::number(
                "max_body_bytes",
                "max_body_bytes",
                server.max_body_bytes.to_string(),
            ),
            FormField::number(
                "connect_timeout_ms",
                "connect_timeout_ms",
                server.connect_timeout_ms.to_string(),
            ),
            FormField::number(
                "first_byte_timeout_ms",
                "first_byte_timeout_ms",
                server.first_byte_timeout_ms.to_string(),
            ),
            FormField::number(
                "stream_idle_timeout_ms",
                "stream_idle_timeout_ms",
                server.stream_idle_timeout_ms.to_string(),
            ),
            FormField::boolean(
                "log_request_bodies",
                "log_request_bodies",
                server.log_request_bodies,
            ),
        ];
        Self {
            kind: FormKind::Server,
            title: "Server settings".to_string(),
            fields,
            selected: 0,
            editing: None,
            error: None,
            original: FormOriginal::Server(server.clone()),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> FormAction {
        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(edit) = self.editing.take() {
                self.fields[self.selected].value = edit.into_value();
            }
            return FormAction::Apply;
        }

        if self.editing.is_some() {
            let outcome = self
                .editing
                .as_mut()
                .expect("editing is present")
                .handle_key(key);
            match outcome {
                EditOutcome::Continue => {}
                EditOutcome::Cancel => self.editing = None,
                EditOutcome::Commit => {
                    let edit = self.editing.take().expect("editing is present");
                    self.fields[self.selected].value = edit.into_value();
                }
                EditOutcome::Next => {
                    let edit = self.editing.take().expect("editing is present");
                    self.fields[self.selected].value = edit.into_value();
                    self.move_selection(1);
                }
                EditOutcome::Previous => {
                    let edit = self.editing.take().expect("editing is present");
                    self.fields[self.selected].value = edit.into_value();
                    self.move_selection(-1);
                }
            }
            return FormAction::Continue;
        }

        match key.code {
            KeyCode::Esc => FormAction::Cancel,
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                FormAction::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                FormAction::Continue
            }
            KeyCode::Tab => {
                self.move_selection(1);
                FormAction::Continue
            }
            KeyCode::BackTab => {
                self.move_selection(-1);
                FormAction::Continue
            }
            KeyCode::Left => {
                self.cycle_choice(-1);
                FormAction::Continue
            }
            KeyCode::Right => {
                self.cycle_choice(1);
                FormAction::Continue
            }
            KeyCode::Char(' ') => {
                self.toggle_bool();
                FormAction::Continue
            }
            KeyCode::Enter => {
                if self.fields[self.selected].read_only {
                    return FormAction::Continue;
                }
                match self.fields[self.selected].kind {
                    FieldKind::Bool => {
                        self.toggle_bool();
                    }
                    FieldKind::Choice(_) => {
                        self.cycle_choice(1);
                    }
                    _ => {
                        self.editing =
                            Some(TextEdit::new(self.fields[self.selected].value.clone()));
                    }
                }
                FormAction::Continue
            }
            _ => FormAction::Continue,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.fields.is_empty() {
            return;
        }
        let len = self.fields.len() as isize;
        let current = self.selected as isize;
        let next = (current + delta).rem_euclid(len);
        self.selected = next as usize;
    }

    fn toggle_bool(&mut self) {
        if let FieldKind::Bool = self.fields[self.selected].kind {
            self.fields[self.selected].boolean = !self.fields[self.selected].boolean;
        }
    }

    fn cycle_choice(&mut self, delta: isize) {
        let FieldKind::Choice(options) = self.fields[self.selected].kind else {
            return;
        };
        let len = options.len() as isize;
        if len == 0 {
            return;
        }
        let current = self.fields[self.selected].choice as isize;
        self.fields[self.selected].choice = (current + delta).rem_euclid(len) as usize;
    }

    fn field(&self, key: &str) -> Result<&FormField, String> {
        self.fields
            .iter()
            .find(|field| field.key == key)
            .ok_or_else(|| format!("internal error: missing form field `{key}`"))
    }

    fn text(&self, key: &str) -> Result<&str, String> {
        Ok(self.field(key)?.value.trim())
    }

    fn boolean(&self, key: &str) -> Result<bool, String> {
        Ok(self.field(key)?.boolean)
    }

    fn choice(&self, key: &str) -> Result<&'static str, String> {
        let field = self.field(key)?;
        let FieldKind::Choice(options) = field.kind else {
            return Err(format!("internal error: field `{key}` is not a choice"));
        };
        options
            .get(field.choice)
            .copied()
            .ok_or_else(|| format!("internal error: field `{key}` has no selected option"))
    }

    fn apply(&self, config: &mut Config) -> Result<(), String> {
        match self.kind {
            FormKind::Provider { index } => self.apply_provider(config, index),
            FormKind::Route { index } => self.apply_route(config, index),
            FormKind::Server => self.apply_server(config),
        }
    }

    fn apply_provider(&self, config: &mut Config, index: Option<usize>) -> Result<(), String> {
        let FormOriginal::Provider(mut provider) = self.original.clone() else {
            return Err("internal error: provider form has the wrong original".to_string());
        };
        let is_new = index.is_none();

        if is_new {
            let id = self.text("id")?;
            if id.is_empty() {
                return Err("id must not be empty".to_string());
            }
            if config.providers.iter().any(|provider| provider.id == id) {
                return Err(format!("provider id `{id}` already exists"));
            }
            provider.id = id.to_string();
        }

        provider.name = optional(self.text("name")?);
        provider.base_url = self.text("base_url")?.to_string();
        validate_base_url(&provider.base_url)?;
        provider.wire_api = match self.choice("wire_api")? {
            "chat" => WireApi::Chat,
            "responses" => WireApi::Responses,
            "messages" => WireApi::Messages,
            other => return Err(format!("unknown wire_api `{other}`")),
        };
        provider.api_key_env = optional(self.text("api_key_env")?);
        provider.api_key = optional(self.text("api_key")?);
        provider.auth = match self.choice("auth.kind")? {
            "default" => None,
            "bearer" => Some(AuthConfig {
                kind: AuthKind::Bearer,
                header: optional(self.text("auth.header")?),
                prefix: optional(self.text("auth.prefix")?),
                query_param: None,
            }),
            "header" => Some(AuthConfig {
                kind: AuthKind::Header,
                header: optional(self.text("auth.header")?),
                prefix: optional(self.text("auth.prefix")?),
                query_param: None,
            }),
            "query" => Some(AuthConfig {
                kind: AuthKind::Query,
                header: None,
                prefix: None,
                query_param: optional(self.text("auth.query_param")?),
            }),
            "none" => Some(AuthConfig {
                kind: AuthKind::None,
                header: None,
                prefix: None,
                query_param: None,
            }),
            other => return Err(format!("unknown auth type `{other}`")),
        };
        provider.max_concurrency =
            parse_positive_usize(self.text("max_concurrency")?, "max_concurrency")?;
        provider.max_queue = parse_usize(self.text("max_queue")?, "max_queue")?;
        provider.queue_timeout_ms = parse_u64(self.text("queue_timeout_ms")?, "queue_timeout_ms")?;
        provider.cooldown_ms = parse_u64(self.text("cooldown_ms")?, "cooldown_ms")?;
        provider.failure_threshold =
            parse_positive_u32(self.text("failure_threshold")?, "failure_threshold")?;
        provider.models = parse_model_map(self.text("models")?)?;

        match index {
            Some(index) if index < config.providers.len() => config.providers[index] = *provider,
            Some(_) => return Err("provider no longer exists".to_string()),
            None => config.providers.push(*provider),
        }
        Ok(())
    }

    fn apply_route(&self, config: &mut Config, index: Option<usize>) -> Result<(), String> {
        let FormOriginal::Route(mut route) = self.original.clone() else {
            return Err("internal error: route form has the wrong original".to_string());
        };
        route.model = self.text("model")?.to_string();
        if route.model.is_empty() {
            return Err("model must not be empty".to_string());
        }
        route.chain = parse_comma_list(self.text("chain")?);
        if route.chain.is_empty() {
            return Err("chain must contain at least one provider id".to_string());
        }
        for provider_id in &route.chain {
            if !config
                .providers
                .iter()
                .any(|provider| provider.id == *provider_id)
            {
                return Err(format!("unknown provider `{provider_id}` in chain"));
            }
        }
        if config
            .routes
            .iter()
            .enumerate()
            .any(|(other_index, other)| other.model == route.model && Some(other_index) != index)
        {
            return Err(format!("route for model `{}` already exists", route.model));
        }

        match index {
            Some(index) if index < config.routes.len() => config.routes[index] = route,
            Some(_) => return Err("route no longer exists".to_string()),
            None => config.routes.push(route),
        }
        Ok(())
    }

    fn apply_server(&self, config: &mut Config) -> Result<(), String> {
        let FormOriginal::Server(mut server) = self.original.clone() else {
            return Err("internal error: server form has the wrong original".to_string());
        };
        server.listen = self.text("listen")?.to_string();
        if server.listen.is_empty() {
            return Err("listen must not be empty".to_string());
        }
        server.require_auth = self.boolean("require_auth")?;
        server.api_keys = parse_comma_list(self.text("api_keys")?);
        if server.require_auth && server.api_keys.is_empty() {
            return Err("require_auth is enabled but api_keys is empty".to_string());
        }
        server.max_body_bytes =
            parse_positive_usize(self.text("max_body_bytes")?, "max_body_bytes")?;
        server.connect_timeout_ms =
            parse_positive_u64(self.text("connect_timeout_ms")?, "connect_timeout_ms")?;
        server.first_byte_timeout_ms =
            parse_positive_u64(self.text("first_byte_timeout_ms")?, "first_byte_timeout_ms")?;
        server.stream_idle_timeout_ms = parse_positive_u64(
            self.text("stream_idle_timeout_ms")?,
            "stream_idle_timeout_ms",
        )?;
        server.log_request_bodies = self.boolean("log_request_bodies")?;
        config.server = server;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum FormAction {
    Continue,
    Cancel,
    Apply,
}

#[derive(Debug, Clone, Copy)]
enum PendingAction {
    Start,
    Restart,
}

fn optional(value: &str) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value.trim().to_string())
    }
}

fn validate_base_url(raw: &str) -> Result<(), String> {
    let url = Url::parse(raw).map_err(|error| format!("base_url is invalid: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("base_url must use http or https".to_string());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("base_url must not contain a query or fragment".to_string());
    }
    Ok(())
}

fn parse_usize(raw: &str, label: &str) -> Result<usize, String> {
    raw.parse::<usize>()
        .map_err(|_| format!("{label} must be a non-negative integer"))
}

fn parse_positive_usize(raw: &str, label: &str) -> Result<usize, String> {
    let value = parse_usize(raw, label)?;
    if value == 0 {
        return Err(format!("{label} must be > 0"));
    }
    Ok(value)
}

fn parse_u64(raw: &str, label: &str) -> Result<u64, String> {
    raw.parse::<u64>()
        .map_err(|_| format!("{label} must be a non-negative integer"))
}

fn parse_positive_u64(raw: &str, label: &str) -> Result<u64, String> {
    let value = parse_u64(raw, label)?;
    if value == 0 {
        return Err(format!("{label} must be > 0"));
    }
    Ok(value)
}

fn parse_positive_u32(raw: &str, label: &str) -> Result<u32, String> {
    let value = raw
        .parse::<u32>()
        .map_err(|_| format!("{label} must be a non-negative integer"))?;
    if value == 0 {
        return Err(format!("{label} must be > 0"));
    }
    Ok(value)
}

fn default_provider() -> ProviderConfig {
    ProviderConfig {
        id: "new-provider".to_string(),
        name: Some("New provider".to_string()),
        base_url: "https://relay.example.com/v1".to_string(),
        wire_api: WireApi::Chat,
        api_key: None,
        api_key_env: None,
        auth: None,
        headers: BTreeMap::new(),
        query: BTreeMap::new(),
        models: BTreeMap::new(),
        max_concurrency: 4,
        max_queue: 0,
        queue_timeout_ms: 800,
        cooldown_ms: 30_000,
        failure_threshold: 3,
        retry_after_wait_ms: 1_000,
        compat: CompatConfig::default(),
    }
}

#[allow(dead_code)]
fn _breaker_label(breaker: BreakerStatus) -> &'static str {
    match breaker {
        BreakerStatus::Closed => "closed",
        BreakerStatus::HalfOpen => "half-open",
        BreakerStatus::Open => "open",
    }
}
