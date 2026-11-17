//! Ratatui rendering for the interactive console.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, Wrap};

use crate::provider::{BreakerStatus, ProviderSnapshot};

use super::app::{
    App, ConfirmState, FormState, InfoState, Modal, NoticeKind, Screen, ServicePhase,
};
use super::logbuf::{LogLevel, LogLine};

const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 18;
const LABEL_WIDTH: usize = 20;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        draw_too_small(frame, area);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(area);

    draw_header(frame, app, chunks[0]);
    match app.screen {
        Screen::Dashboard => draw_dashboard(frame, app, chunks[1]),
        Screen::Providers => draw_providers(frame, app, chunks[1], false),
        Screen::Routes => draw_routes(frame, app, chunks[1]),
        Screen::Settings => draw_settings(frame, app, chunks[1]),
        Screen::Logs => draw_logs(frame, app, chunks[1]),
        Screen::Help => draw_help(frame, app, chunks[1]),
    }
    draw_footer(frame, app, chunks[2]);
    draw_modal(frame, app, area);
}

fn draw_too_small(frame: &mut Frame, area: Rect) {
    let text = Text::from(vec![
        Line::from(Span::styled(
            "Terminal too small",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "Need at least {MIN_WIDTH}x{MIN_HEIGHT}, current {}x{}",
            area.width, area.height
        )),
    ]);
    let paragraph = Paragraph::new(text).alignment(Alignment::Center).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Tomato Router "),
    );
    frame.render_widget(paragraph, area);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let mut spans = vec![
        Span::styled(
            " Tomato Router ",
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("  "),
        Span::styled(
            format!(" {} ", app.service.phase.label()),
            phase_style(app.service.phase),
        ),
    ];

    if let Some(listen) = app.service.listen {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            listen.to_string(),
            Style::default().fg(Color::LightCyan),
        ));
    }
    spans.push(Span::styled(
        format!("  config: {}", app.config_path.display()),
        Style::default().fg(Color::Gray),
    ));
    if app.draft.as_ref().is_some_and(|draft| draft.is_dirty()) {
        spans.push(Span::styled(
            "  [unsaved]",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if app.pending_restart() {
        spans.push(Span::styled(
            "  [restart pending]",
            Style::default().fg(Color::LightYellow),
        ));
    }
    if app.load_error.is_some() {
        spans.push(Span::styled(
            "  [config error]",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    if app.listen_override.is_some() {
        spans.push(Span::styled(
            "  [--listen override]",
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), rows[0]);

    let mut tabs = vec![Span::raw(" ")];
    for (index, screen) in Screen::ALL.iter().enumerate() {
        let style = if *screen == app.screen {
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightRed)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        tabs.push(Span::styled(
            format!(" {} {} ", index + 1, screen.title()),
            style,
        ));
    }
    tabs.push(Span::styled(
        "   ? help",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(Line::from(tabs)), rows[1]);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let notice_line = if let Some(notice) = &app.notice {
        let (symbol, style) = match notice.kind {
            NoticeKind::Info => ("i", Style::default().fg(Color::LightCyan)),
            NoticeKind::Success => ("+", Style::default().fg(Color::LightGreen)),
            NoticeKind::Warning => ("!", Style::default().fg(Color::Yellow)),
            NoticeKind::Error => ("x", Style::default().fg(Color::LightRed)),
        };
        Line::from(vec![
            Span::styled(format!(" {symbol} "), style.add_modifier(Modifier::BOLD)),
            Span::styled(notice.text.clone(), style),
        ])
    } else if let Some(error) = &app.service.last_error {
        Line::from(Span::styled(
            format!(" x {error}"),
            Style::default().fg(Color::LightRed),
        ))
    } else {
        Line::from(Span::styled(
            " Ready. Changes are kept in memory until you save.",
            Style::default().fg(Color::DarkGray),
        ))
    };
    frame.render_widget(Paragraph::new(notice_line), rows[0]);

    let keys = match app.screen {
        Screen::Dashboard => {
            " s start   x stop   r restart   w save   v validate   R reload   F5 apply   q quit"
        }
        Screen::Providers => {
            " a add   Enter edit   d delete   Ctrl+Up/Down reorder   w save   v validate   q quit"
        }
        Screen::Routes => " a add   Enter edit   d delete   w save   v validate   q quit",
        Screen::Settings => " Enter edit server settings   w save   v validate   q quit",
        Screen::Logs => " j/k scroll   PgUp/PgDn page   f follow   l level   c clear   q quit",
        Screen::Help => " j/k scroll   PgUp/PgDn page   Esc back   q quit",
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            keys,
            Style::default().fg(Color::DarkGray),
        ))),
        rows[1],
    );
}

fn draw_dashboard(frame: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Min(4),
        ])
        .split(area);

    draw_overview(frame, app, chunks[0]);
    draw_traffic(frame, app, chunks[1]);
    draw_providers(frame, app, chunks[2], true);
}

fn draw_overview(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();

    lines.push(status_line(
        "Config",
        app.config_path.display().to_string(),
        if app.draft.as_ref().is_some_and(|draft| draft.is_dirty()) {
            Color::Yellow
        } else {
            Color::Gray
        },
    ));
    let service = match app.service.phase {
        ServicePhase::Running => match app.service.listen {
            Some(listen) => format!("RUNNING on {listen}"),
            None => "RUNNING".to_string(),
        },
        phase => phase.label().to_string(),
    };
    lines.push(status_line(
        "Service",
        service,
        phase_color(app.service.phase),
    ));

    let (uptime, total, rate, active) = match &app.service.state {
        Some(state) => (
            format_duration(state.uptime_seconds()),
            state.metrics.requests_total().to_string(),
            app.request_total_last_minute().to_string(),
            app.active_request_count().to_string(),
        ),
        None => (
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
        ),
    };
    lines.push(status_line(
        "Traffic",
        format!("uptime {uptime}   requests {total}   last 60s {rate}   active {active}"),
        Color::Gray,
    ));

    let config_state = match (&app.draft, &app.load_error) {
        (Some(draft), _) => {
            let report = draft.report();
            if !report.errors.is_empty() {
                format!("{} error(s)", report.errors.len())
            } else if !report.warnings.is_empty() {
                format!("{} warning(s); press v for details", report.warnings.len())
            } else {
                "OK".to_string()
            }
        }
        (None, Some(_)) => "unavailable".to_string(),
        (None, None) => "unavailable".to_string(),
    };
    lines.push(status_line(
        "Validation",
        config_state,
        if app.draft.is_none() {
            Color::Red
        } else {
            Color::Gray
        },
    ));

    let (providers, routes, auth) = match app.active_config() {
        Some(config) => (
            config.providers.len(),
            config.routes.len(),
            if config.server.require_auth {
                format!("required ({} key(s))", config.server.api_keys.len())
            } else {
                "disabled".to_string()
            },
        ),
        None => (0, 0, "-".to_string()),
    };
    lines.push(status_line(
        "Inventory",
        format!("{providers} provider(s)   {routes} route(s)   local auth {auth}"),
        Color::Gray,
    ));

    if let Some(error) = &app.load_error {
        lines.push(Line::from(Span::styled(
            format!("  {error}"),
            Style::default().fg(Color::LightRed),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "  s start   x stop   r restart   w save   v validate   F5 apply",
            Style::default().fg(Color::DarkGray),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Overview ")),
        area,
    );
}

fn draw_traffic(frame: &mut Frame, app: &App, area: Rect) {
    let data: Vec<u64> = app.request_samples.iter().copied().collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Requests per second (last 60s) ");
    if data.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                " no samples yet",
                Style::default().fg(Color::DarkGray),
            ))
            .block(block),
            area,
        );
    } else {
        let max = data.iter().copied().max().unwrap_or(1).max(1);
        let sparkline = Sparkline::default()
            .block(block)
            .data(&data)
            .max(max)
            .style(Style::default().fg(Color::LightCyan));
        frame.render_widget(sparkline, area);
    }
}

fn draw_providers(frame: &mut Frame, app: &mut App, area: Rect, compact: bool) {
    let Some(draft) = &app.draft else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "No configuration loaded. Press R to reload or c to create one from the example.",
                Style::default().fg(Color::Yellow),
            ))
            .block(Block::default().borders(Borders::ALL).title(" Providers "))
            .wrap(Wrap { trim: false }),
            area,
        );
        return;
    };

    let snapshots: HashMap<String, ProviderSnapshot> = app
        .provider_snapshots()
        .into_iter()
        .map(|snapshot| (snapshot.id.clone(), snapshot))
        .collect();

    let mut rows = Vec::new();
    for (index, provider) in draft.config.providers.iter().enumerate() {
        let snapshot = snapshots.get(provider.id.as_str());
        let breaker = snapshot
            .map(|snapshot| breaker_text(snapshot.breaker, snapshot.cooldown_ms_remaining))
            .unwrap_or_else(|| "-".to_string());
        let inflight = snapshot
            .map(|snapshot| format!("{}/{}", snapshot.inflight, snapshot.max_concurrency))
            .unwrap_or_else(|| format!("- /{}", provider.max_concurrency));
        let queue = snapshot
            .map(|snapshot| format!("{}/{}", snapshot.queue_depth, snapshot.max_queue))
            .unwrap_or_else(|| format!("- /{}", provider.max_queue));
        let (ok, fail) = snapshot
            .map(|snapshot| {
                (
                    snapshot.successes.to_string(),
                    snapshot.failures.to_string(),
                )
            })
            .unwrap_or_else(|| ("-".to_string(), "-".to_string()));
        let last_error = snapshot
            .and_then(|snapshot| snapshot.last_error.as_ref())
            .map(|error| format!("{} ({}s ago)", error.kind, error.age_ms / 1000))
            .unwrap_or_else(|| "-".to_string());

        let breaker_style = snapshot
            .map(|snapshot| breaker_style(snapshot.breaker))
            .unwrap_or_else(|| Style::default().fg(Color::DarkGray));
        let mut id_cell = Cell::from(provider.id.clone());
        if index == app.provider_selection {
            id_cell = id_cell.style(Style::default().add_modifier(Modifier::BOLD));
        }

        if compact {
            rows.push(Row::new(vec![
                id_cell,
                Cell::from(provider.wire_api.to_string()),
                Cell::from(breaker).style(breaker_style),
                Cell::from(inflight),
                Cell::from(format!("{ok}/{fail}")),
            ]));
        } else {
            rows.push(Row::new(vec![
                id_cell,
                Cell::from(provider.display_name().to_string()),
                Cell::from(provider.wire_api.to_string()),
                Cell::from(breaker).style(breaker_style),
                Cell::from(inflight),
                Cell::from(queue),
                Cell::from(ok),
                Cell::from(fail),
                Cell::from(last_error),
            ]));
        }
    }

    let (widths, header) = if compact {
        (
            vec![
                Constraint::Length(20),
                Constraint::Length(10),
                Constraint::Length(12),
                Constraint::Length(10),
                Constraint::Min(8),
            ],
            Row::new(vec!["id", "wire", "breaker", "active", "ok/fail"]),
        )
    } else {
        (
            vec![
                Constraint::Length(20),
                Constraint::Length(18),
                Constraint::Length(10),
                Constraint::Length(12),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(7),
                Constraint::Length(7),
                Constraint::Min(12),
            ],
            Row::new(vec![
                "id",
                "name",
                "wire",
                "breaker",
                "inflight",
                "queue",
                "ok",
                "fail",
                "last error",
            ]),
        )
    };

    let title = if compact {
        " Provider health "
    } else {
        " Providers (a add, Enter edit, d delete, Ctrl+Up/Down reorder) "
    };
    let table = Table::new(rows, widths)
        .header(header.style(Style::default().fg(Color::DarkGray)))
        .block(Block::default().borders(Borders::ALL).title(title))
        .row_highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 40, 50))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    if app.provider_selection >= draft.config.providers.len() {
        app.provider_selection = draft.config.providers.len().saturating_sub(1);
    }
    app.provider_table.select(Some(
        app.provider_selection
            .min(draft.config.providers.len().saturating_sub(1)),
    ));
    frame.render_stateful_widget(table, area, &mut app.provider_table);
}

fn draw_routes(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(draft) = &app.draft else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "No configuration loaded.",
                Style::default().fg(Color::Yellow),
            ))
            .block(Block::default().borders(Borders::ALL).title(" Routes ")),
            area,
        );
        return;
    };

    if draft.config.routes.is_empty() {
        let order = draft
            .config
            .providers
            .iter()
            .map(|provider| provider.id.as_str())
            .collect::<Vec<_>>()
            .join(" -> ");
        let text = Text::from(vec![
            Line::from("No explicit routes. Provider declaration order is the default chain:"),
            Line::from(""),
            Line::from(Span::styled(
                format!("  {order}"),
                Style::default().fg(Color::LightCyan),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Press a to add a route. Without routes, provider declaration order is used.",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        frame.render_widget(
            Paragraph::new(text)
                .block(Block::default().borders(Borders::ALL).title(" Routes "))
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    let rows = draft.config.routes.iter().map(|route| {
        Row::new(vec![
            Cell::from(route.model.clone()),
            Cell::from(route.chain.join(" -> ")),
        ])
    });
    let table = Table::new(rows, [Constraint::Length(28), Constraint::Min(20)])
        .header(
            Row::new(vec!["model", "chain (strict priority)"])
                .style(Style::default().fg(Color::DarkGray)),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Routes (a add, Enter edit, d delete) "),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 40, 50))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    if app.route_selection >= draft.config.routes.len() {
        app.route_selection = draft.config.routes.len().saturating_sub(1);
    }
    app.route_table.select(Some(
        app.route_selection
            .min(draft.config.routes.len().saturating_sub(1)),
    ));
    frame.render_stateful_widget(table, area, &mut app.route_table);
}

fn draw_settings(frame: &mut Frame, app: &App, area: Rect) {
    let Some(config) = app.active_config() else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "No configuration loaded.",
                Style::default().fg(Color::Yellow),
            ))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Server settings "),
            ),
            area,
        );
        return;
    };

    let server = &config.server;
    let mut lines = vec![
        setting_line("listen", server.listen.clone()),
        setting_line(
            "require_auth",
            if server.require_auth { "yes" } else { "no" }.to_string(),
        ),
        setting_line(
            "api_keys",
            format!("{} configured (values hidden)", server.api_keys.len()),
        ),
        setting_line("max_body_bytes", server.max_body_bytes.to_string()),
        setting_line("connect_timeout_ms", server.connect_timeout_ms.to_string()),
        setting_line(
            "first_byte_timeout_ms",
            server.first_byte_timeout_ms.to_string(),
        ),
        setting_line(
            "stream_idle_timeout_ms",
            server.stream_idle_timeout_ms.to_string(),
        ),
        setting_line(
            "log_request_bodies",
            if server.log_request_bodies {
                "yes (prompts may appear in Logs)"
            } else {
                "no"
            }
            .to_string(),
        ),
        Line::from(""),
        Line::from(Span::styled(
            "Press Enter to edit. API keys are masked; inline keys are written to disk.",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    if !server.require_auth && !is_loopback(&server.listen) {
        lines.push(Line::from(Span::styled(
            "WARNING: listening on a non-loopback address without local authentication.",
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Server settings "),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_logs(frame: &mut Frame, app: &App, area: Rect) {
    let logs = app.filtered_logs();
    let inner_height = area.height.saturating_sub(2) as usize;
    let visible = inner_height.max(1);

    let (start, end) = if app.log_follow {
        let end = logs.len();
        (end.saturating_sub(visible), end)
    } else {
        let end = logs.len().saturating_sub(app.log_back);
        (end.saturating_sub(visible), end)
    };
    let lines: Vec<Line> = logs[start..end].iter().map(log_line).collect::<Vec<_>>();

    let mode = if app.log_follow { "follow" } else { "paused" };
    let title = format!(
        " Logs [{}, {mode}] {}/{}  unseen {}  dropped {} ",
        app.log_level.as_str(),
        logs.len(),
        app.log_buffer.len(),
        app.log_unseen,
        app.log_buffer.dropped()
    );
    let mut paragraph =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title.clone()));
    if logs.is_empty() {
        paragraph = Paragraph::new(Span::styled(
            "No log lines at this level yet.",
            Style::default().fg(Color::DarkGray),
        ))
        .block(Block::default().borders(Borders::ALL).title(title));
    }
    frame.render_widget(paragraph, area);
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let lines = HELP_LINES
        .iter()
        .map(|line| {
            if let Some(heading) = line.strip_prefix("# ") {
                Line::from(Span::styled(
                    heading,
                    Style::default()
                        .fg(Color::LightRed)
                        .add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(*line)
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Help "))
            .scroll((app.help_scroll, 0)),
        area,
    );
}

fn draw_modal(frame: &mut Frame, app: &App, area: Rect) {
    let Some(modal) = &app.modal else {
        return;
    };
    match modal {
        Modal::Confirm(confirm) => draw_confirm(frame, confirm, area),
        Modal::Info(info) => draw_info(frame, info, area),
        Modal::Form(form) => draw_form(frame, form, area),
    }
}

fn draw_confirm(frame: &mut Frame, confirm: &ConfirmState, area: Rect) {
    let popup = centered(area, 72, 9);
    frame.render_widget(Clear, popup);
    let text = Text::from(vec![
        Line::from(confirm.message.clone()),
        Line::from(""),
        Line::from(Span::styled(
            "Enter / y  confirm      Esc / q  cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", confirm.title))
                    .title_style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_info(frame: &mut Frame, info: &InfoState, area: Rect) {
    let height = (info.lines.len() as u16 + 4).clamp(8, area.height.saturating_sub(2));
    let popup = centered(area, 96, height);
    frame.render_widget(Clear, popup);
    let mut lines = info
        .lines
        .iter()
        .map(|line| Line::from(line.clone()))
        .collect::<Vec<_>>();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Esc / Enter  close      j/k / PgUp/PgDn  scroll",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", info.title)),
            )
            .wrap(Wrap { trim: false })
            .scroll((info.scroll, 0)),
        popup,
    );
}

fn draw_form(frame: &mut Frame, form: &FormState, area: Rect) {
    let height = (form.fields.len() as u16 + 4).clamp(10, area.height.saturating_sub(2));
    let popup = centered(area, 96, height);
    frame.render_widget(Clear, popup);

    let inner = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", form.title))
        .title_style(
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        )
        .inner(popup);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(inner);

    let visible = chunks[0].height as usize;
    let selected = form.selected.min(form.fields.len().saturating_sub(1));
    let start = if form.fields.len() <= visible {
        0
    } else {
        selected
            .saturating_sub(visible / 2)
            .min(form.fields.len() - visible)
    };
    let end = (start + visible).min(form.fields.len());

    let mut lines = Vec::new();
    for (index, field) in form.fields.iter().enumerate().take(end).skip(start) {
        let is_selected = index == selected;
        let row_style = if is_selected {
            Style::default()
                .bg(Color::Rgb(45, 45, 55))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let label_style = if field.read_only {
            Style::default().fg(Color::DarkGray)
        } else if is_selected {
            Style::default().fg(Color::Black)
        } else {
            Style::default().fg(Color::LightCyan)
        };

        let value = if is_selected && let Some(edit) = &form.editing {
            let cursor = edit.cursor().min(edit.value().len());
            let (before, after) = edit.value().split_at(cursor);
            format!("{before}\u{258c}{after}")
        } else {
            field.display_value()
        };

        let mut spans = vec![
            Span::styled(
                format!("{:<LABEL_WIDTH$}", field.label),
                label_style.patch(row_style),
            ),
            Span::styled(value, Style::default().fg(Color::White).patch(row_style)),
        ];
        if field.read_only {
            spans.push(Span::styled(
                "  (read-only)",
                Style::default().fg(Color::DarkGray).patch(row_style),
            ));
        } else if is_selected && form.editing.is_none() && !field.hint.is_empty() {
            spans.push(Span::styled(
                format!("  {}", field.hint),
                Style::default().fg(Color::DarkGray).patch(row_style),
            ));
        }
        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(lines), chunks[0]);

    let mut footer = Vec::new();
    if let Some(error) = &form.error {
        footer.push(Line::from(Span::styled(
            format!(" {error}"),
            Style::default().fg(Color::LightRed),
        )));
    } else {
        footer.push(Line::from(Span::styled(
            " Enter edit/toggle   Space toggle   Left/Right choice   Tab next   Ctrl+S apply",
            Style::default().fg(Color::DarkGray),
        )));
    }
    footer.push(Line::from(Span::styled(
        " Esc cancel",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(footer), chunks[1]);
}

fn log_line(line: &LogLine) -> Line<'static> {
    let color = match line.level {
        LogLevel::Error => Color::LightRed,
        LogLevel::Warn => Color::LightYellow,
        LogLevel::Info => Color::Gray,
        LogLevel::Debug => Color::DarkGray,
        LogLevel::Trace => Color::DarkGray,
    };
    Line::from(vec![
        Span::styled(
            format!("{} ", line.time),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("{:<5} ", line.level.as_str()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{}: ", line.target),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(line.message.clone(), Style::default().fg(color)),
    ])
}

fn status_line(label: &str, value: String, value_color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {label:<12}"),
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(value, Style::default().fg(value_color)),
    ])
}

fn setting_line(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {label:<26}"),
            Style::default().fg(Color::LightCyan),
        ),
        Span::styled(value, Style::default().fg(Color::White)),
    ])
}

fn phase_color(phase: ServicePhase) -> Color {
    match phase {
        ServicePhase::Stopped => Color::DarkGray,
        ServicePhase::Starting | ServicePhase::Stopping => Color::Yellow,
        ServicePhase::Running => Color::LightGreen,
        ServicePhase::Failed => Color::LightRed,
    }
}

fn phase_style(phase: ServicePhase) -> Style {
    let base = Style::default()
        .fg(Color::Black)
        .bg(phase_color(phase))
        .add_modifier(Modifier::BOLD);
    if phase == ServicePhase::Stopped {
        Style::default()
            .fg(Color::Gray)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        base
    }
}

fn breaker_text(breaker: BreakerStatus, cooldown_ms: Option<u64>) -> String {
    match breaker {
        BreakerStatus::Closed => "closed".to_string(),
        BreakerStatus::HalfOpen => "half-open".to_string(),
        BreakerStatus::Open => match cooldown_ms {
            Some(ms) => format!("open {}s", ms.div_ceil(1000)),
            None => "open".to_string(),
        },
    }
}

fn breaker_style(breaker: BreakerStatus) -> Style {
    match breaker {
        BreakerStatus::Closed => Style::default().fg(Color::LightGreen),
        BreakerStatus::HalfOpen => Style::default().fg(Color::Yellow),
        BreakerStatus::Open => Style::default().fg(Color::LightRed),
    }
}

fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn is_loopback(listen: &str) -> bool {
    crate::config::is_loopback_listen(listen)
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

const HELP_LINES: &[&str] = &[
    "# Tomato Router TUI",
    "",
    "The gateway runs inside this process. Quitting the TUI stops it.",
    "",
    "# Service",
    "  s             Start the gateway from the saved configuration",
    "  x             Stop the gateway (5s drain, then force stop)",
    "  r             Restart the gateway",
    "  F5            Save if needed, then start or restart",
    "  w             Save the configuration to disk",
    "  v             Show validation summary, errors and warnings",
    "  R             Reload the configuration from disk",
    "  q / Ctrl+C    Quit (confirms when the gateway is running)",
    "",
    "# Configuration",
    "  Providers     a add, Enter edit, d delete, Ctrl+Up/Down reorder",
    "  Routes        a add, Enter edit, d delete",
    "  Settings      Enter edit server settings",
    "",
    "Start and restart always reload the saved file. If the draft is dirty,",
    "the TUI offers to save first.",
    "",
    "# Logs",
    "  j/k           Scroll",
    "  PgUp/PgDn     Page",
    "  f             Toggle follow mode",
    "  l             Cycle minimum level",
    "  c             Clear the in-memory buffer",
    "",
    "# Files",
    "  Saving writes a full TOML document and keeps a timestamped backup:",
    "  config/tomato.toml.bak-tomato-<unix-seconds>",
    "",
    "  Comments and unknown fields are not preserved by the full rewrite.",
    "",
    "# Security",
    "  Inline API keys are masked on screen but stored in plain text.",
    "  Config backups can contain secrets; protect or delete them.",
];

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc;

    use crate::config::Config;

    use super::super::app::{ConfirmAction, UiInit};
    use super::super::draft::{Draft, LoadedDraft};
    use super::super::logbuf::LogBuffer;
    use super::*;

    fn test_app() -> App {
        let config: Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:8787"
            require_auth = true
            api_keys = ["sk-test"]

            [[providers]]
            id = "primary"
            name = "Primary"
            base_url = "https://relay.example/v1"
            api_key = "sk-test"

            [[providers]]
            id = "backup"
            base_url = "https://backup.example/v1"
            api_key_env = "BACKUP_KEY"

            [[routes]]
            model = "tomato-codex"
            chain = ["primary", "backup"]
            "#,
        )
        .unwrap();
        let path = PathBuf::from("config/tomato.toml");
        let loaded = LoadedDraft {
            path: path.clone(),
            draft: Some(Draft {
                path,
                saved_config: config.clone(),
                config,
            }),
            error: None,
            missing: false,
        };
        let (command_tx, _command_rx) = mpsc::unbounded_channel();
        let (_event_tx, event_rx) = mpsc::unbounded_channel();
        App::new(UiInit {
            loaded,
            listen_override: None,
            command_tx,
            event_rx,
            log_buffer: LogBuffer::new(16),
            autostart: false,
        })
    }

    fn render(app: &mut App, width: u16, height: u16) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
    }

    #[test]
    fn renders_every_screen_and_modal() {
        let mut app = test_app();
        app.log_buffer.push(LogLine {
            time: "12:00:00".to_string(),
            level: LogLevel::Info,
            target: "test".to_string(),
            message: "hello".to_string(),
        });

        for screen in Screen::ALL {
            app.screen = screen;
            render(&mut app, 120, 36);
        }

        app.modal = Some(Modal::Confirm(Box::new(ConfirmState {
            title: "Confirm".to_string(),
            message: "Proceed?".to_string(),
            action: ConfirmAction::Stop,
        })));
        render(&mut app, 120, 36);

        app.modal = Some(Modal::Info(InfoState {
            title: "Info".to_string(),
            lines: vec!["one".to_string(), "two".to_string()],
            scroll: 0,
        }));
        render(&mut app, 120, 36);

        let config = app.draft.as_ref().unwrap().config.clone();
        app.modal = Some(Modal::Form(Box::new(FormState::provider(None, &config))));
        render(&mut app, 120, 36);
        app.modal = Some(Modal::Form(Box::new(FormState::route(None, &config))));
        render(&mut app, 120, 36);
        app.modal = Some(Modal::Form(Box::new(FormState::server(&config.server))));
        render(&mut app, 120, 36);
    }

    #[test]
    fn renders_small_terminal_and_out_of_range_log_scroll() {
        let mut app = test_app();
        render(&mut app, 40, 10);

        app.screen = Screen::Logs;
        app.log_back = 50;
        app.log_follow = false;
        render(&mut app, 80, 24);
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(75), "1m 15s");
        assert_eq!(format_duration(3_725), "1h 02m 05s");
    }

    #[test]
    fn centers_popups_inside_the_frame() {
        let area = Rect::new(0, 0, 100, 30);
        let popup = centered(area, 80, 10);
        assert_eq!(popup.width, 80);
        assert_eq!(popup.height, 10);
        assert_eq!(popup.x, 10);
        assert_eq!(popup.y, 10);
    }
}
