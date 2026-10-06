use std::{
    collections::VecDeque,
    io::{self, stdout, IsTerminal},
    path::Path,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use crossterm::{
    cursor::Show,
    event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Terminal,
};
use tokio::{
    net::UnixStream,
    signal::unix::{signal, SignalKind},
    sync::mpsc,
    task::JoinSet,
    time,
};

use super::protocol::{self, Request, Response};
use crate::{
    core::service_manager::{RuntimeSnapshot, ServiceState},
    logging::{LogEntry, LogFilter},
};

const LOG_LIMIT: usize = 1000;
const REFRESH: Duration = Duration::from_secs(1);
const FRAME_INTERVAL: Duration = Duration::from_millis(100);

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<(Self, Terminal<CrosstermBackend<io::Stdout>>)> {
        enable_raw_mode().context("cannot enable terminal raw mode")?;
        if let Err(error) = execute!(stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error).context("cannot enter alternate screen");
        }
        let guard = Self;
        let terminal =
            Terminal::new(CrosstermBackend::new(stdout())).context("cannot initialize terminal")?;
        Ok((guard, terminal))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen, Show);
    }
}

struct App {
    snapshot: RuntimeSnapshot,
    selected: usize,
    logs: VecDeque<LogEntry>,
    log_offset: usize,
    confirming_stop: bool,
    busy: bool,
    stopping: bool,
    notice: String,
    color: bool,
}

impl App {
    fn new(snapshot: RuntimeSnapshot, entries: Vec<LogEntry>) -> Self {
        let mut app = Self {
            snapshot,
            selected: 0,
            logs: VecDeque::new(),
            log_offset: 0,
            confirming_stop: false,
            busy: false,
            stopping: false,
            notice: String::new(),
            color: true,
        };
        for entry in entries {
            app.push_log(entry);
        }
        app
    }

    fn push_log(&mut self, entry: LogEntry) {
        if self.logs.len() == LOG_LIMIT {
            self.logs.pop_front();
        }
        self.logs.push_back(entry);
        if self.log_offset > 0 {
            self.log_offset = self
                .log_offset
                .saturating_add(1)
                .min(self.logs.len().saturating_sub(1));
        }
    }

    fn selected_name(&self) -> Option<String> {
        self.snapshot.services.keys().nth(self.selected).cloned()
    }

    fn update_snapshot(&mut self, snapshot: RuntimeSnapshot) {
        let selected = self.selected_name();
        self.snapshot = snapshot;
        self.selected = selected
            .and_then(|name| self.snapshot.services.keys().position(|key| key == &name))
            .unwrap_or_else(|| {
                self.selected
                    .min(self.snapshot.services.len().saturating_sub(1))
            });
    }

    fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        if key.code == KeyCode::Char('q')
            || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c'))
        {
            return Action::Quit;
        }
        if self.confirming_stop {
            return match key.code {
                KeyCode::Char('s') | KeyCode::Enter if !self.busy => {
                    self.confirming_stop = false;
                    self.busy = true;
                    Action::Stop
                }
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.confirming_stop = false;
                    Action::None
                }
                _ => Action::None,
            };
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected =
                    (self.selected + 1).min(self.snapshot.services.len().saturating_sub(1));
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                Action::None
            }
            KeyCode::PageUp => {
                self.log_offset = self
                    .log_offset
                    .saturating_add(10)
                    .min(self.logs.len().saturating_sub(1));
                Action::None
            }
            KeyCode::PageDown => {
                self.log_offset = self.log_offset.saturating_sub(10);
                Action::None
            }
            KeyCode::End => {
                self.log_offset = 0;
                Action::None
            }
            KeyCode::Char('r') if !self.busy => {
                if let Some(name) = self.selected_name() {
                    self.busy = true;
                    self.notice = format!("Restarting {name}...");
                    Action::Restart(name)
                } else {
                    Action::None
                }
            }
            KeyCode::Char('s') if !self.busy => {
                self.confirming_stop = true;
                Action::None
            }
            _ => Action::None,
        }
    }
}

enum Action {
    None,
    Quit,
    Restart(String),
    Stop,
}

enum ControlResult {
    Restart(String, Result<Response>),
    Stop(Result<Response>),
}

pub(super) async fn run(socket: &Path, color: bool) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("top requires an interactive terminal (TTY)");
    }
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    tokio::select! {
        result = run_view(socket, color) => result,
        _ = interrupt.recv() => Ok(()),
        _ = terminate.recv() => Ok(()),
        _ = hangup.recv() => Ok(()),
    }
}

async fn status(socket: &Path) -> Result<RuntimeSnapshot> {
    match time::timeout(
        protocol::IO_TIMEOUT,
        protocol::request(socket, Request::Status),
    )
    .await
    .context("status query timed out")??
    {
        Response::Status(snapshot) => Ok(snapshot),
        _ => bail!("unexpected status response"),
    }
}

async fn run_view(socket: &Path, color: bool) -> Result<()> {
    let snapshot = status(socket).await?;
    let (mut logs, entries) = follow(socket).await?;
    let mut app = App::new(snapshot, entries);
    app.color = color;
    let (_guard, mut terminal) = TerminalGuard::enter()?;
    let mut events = EventStream::new();
    let mut interval = time::interval(FRAME_INTERVAL);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let (status_tx, mut status_rx) = mpsc::channel(1);
    let (log_tx, mut log_rx) = mpsc::channel(128);
    // A partially consumed length-prefixed frame must never be cancelled by
    // a key press or a redraw. Own stream reads in a dedicated task.
    let mut readers = JoinSet::new();
    readers.spawn(async move {
        loop {
            match protocol::next_response(&mut logs).await? {
                Some(Response::Log(entry)) => {
                    if log_tx.send(entry).await.is_err() {
                        return Ok(());
                    }
                }
                Some(Response::Error(error)) => bail!("log stream: {error}"),
                None => bail!("supervisor closed the log stream"),
                _ => bail!("unexpected log stream response"),
            }
        }
    });
    let status_socket = socket.to_path_buf();
    readers.spawn(async move {
        loop {
            time::sleep(REFRESH).await;
            if status_tx.send(status(&status_socket).await?).await.is_err() {
                return Ok(());
            }
        }
    });
    // JoinSet aborts outstanding requests when the view is dropped. Requests
    // already accepted by the supervisor retain their normal lifecycle.
    let mut controls = JoinSet::new();
    let mut dirty = true;
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if dirty { terminal.draw(|frame| render(frame, &app))?; dirty = false; }
                continue;
            }
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) => match app.key(key) {
                    Action::None => {},
                    Action::Quit => return Ok(()),
                    Action::Restart(name) => {
                        let socket = socket.to_path_buf();
                        controls.spawn(async move {
                            let result = protocol::request(&socket, Request::Restart { service: name.clone() }).await;
                            ControlResult::Restart(name, result)
                        });
                    }
                    Action::Stop => {
                        app.stopping = true;
                        app.notice = "Stopping stack...".into();
                        let socket = socket.to_path_buf();
                        controls.spawn(async move { ControlResult::Stop(protocol::request(&socket, Request::Stop).await) });
                    }
                },
                Some(Ok(_)) => {},
                Some(Err(error)) => return Err(error).context("terminal input failed"),
                None => bail!("terminal input ended"),
            },
            Some(snapshot) = status_rx.recv() => app.update_snapshot(snapshot),
            Some(entry) = log_rx.recv() => app.push_log(entry),
            Some(result) = readers.join_next(), if !app.stopping => { result.context("monitor task failed")??; bail!("monitor task ended"); },
            Some(result) = controls.join_next() => match result.context("control task failed")? {
                ControlResult::Restart(name, result) => {
                    app.busy = false;
                    app.notice = match result {
                        Ok(Response::Restarted(_)) => format!("Restarted {name}"),
                        Ok(_) => "Unexpected restart response".into(),
                        Err(error) => format!("Restart failed: {error}"),
                    };
                }
                ControlResult::Stop(result) => match result {
                    Ok(Response::Stopping) => return Ok(()),
                    Ok(_) => { app.busy = false; app.stopping = false; app.notice = "Unexpected stop response".into(); },
                    Err(error) => { app.busy = false; app.stopping = false; app.notice = format!("Stop failed: {error}"); },
                }
            },
        }
        // Rendering is capped even during a continuous stream of log entries.
        dirty = true;
    }
}

async fn follow(socket: &Path) -> Result<(UnixStream, Vec<LogEntry>)> {
    let mut stream = protocol::connect(socket).await?;
    protocol::write(
        &mut stream,
        &Request::FollowLogs {
            service: None,
            tail: 100,
            filter: LogFilter::default(),
        },
    )
    .await?;
    match time::timeout(protocol::IO_TIMEOUT, protocol::next_response(&mut stream))
        .await
        .context("log stream did not start")??
    {
        Some(Response::Logs(entries)) => Ok((stream, entries)),
        Some(Response::Error(error)) => bail!("{error}"),
        _ => bail!("unexpected log stream response"),
    }
}

fn render(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    if area.width < 60 || area.height < 12 {
        frame.render_widget(Paragraph::new("Terminal too small (minimum 60x12)"), area);
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Percentage(42),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(area);
    let title = Line::from(vec![
        Span::styled(
            "devd top",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  supervisor {}  |  {} services",
            app.snapshot.supervisor_pid,
            app.snapshot.services.len()
        )),
    ]);
    frame.render_widget(Paragraph::new(title), rows[0]);
    let table_rows = app.snapshot.services.iter().map(|(name, state)| {
        let status_color = match state.status {
            ServiceState::Healthy | ServiceState::Running => Color::Green,
            ServiceState::Failed | ServiceState::Unhealthy => Color::Red,
            ServiceState::Stopped => Color::DarkGray,
            _ => Color::Yellow,
        };
        Row::new(vec![
            Cell::from(safe_text(name)),
            Cell::from(format!("{:?}", state.status)).style(Style::default().fg(status_color)),
            Cell::from(state.pid.map_or_else(|| "-".into(), |pid| pid.to_string())),
            Cell::from(state.restart_count.to_string()),
            Cell::from(
                state
                    .resources
                    .as_ref()
                    .and_then(|r| r.cpu_percent)
                    .map_or_else(|| "-".into(), |cpu| format!("{cpu:.1}")),
            ),
            Cell::from(state.resources.as_ref().map_or_else(
                || "-".into(),
                |r| format!("{:.1}", r.memory_bytes as f64 / 1_048_576.0),
            )),
        ])
    });
    let table = Table::new(
        table_rows,
        [
            Constraint::Percentage(32),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(9),
        ],
    )
    .header(
        Row::new(["SERVICE", "STATE", "PID", "RESTARTS", "CPU %", "RSS MiB"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().title(" Services ").borders(Borders::ALL))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, rows[1], &mut state);
    let height = rows[2].height.saturating_sub(2) as usize;
    let end = app.logs.len().saturating_sub(app.log_offset);
    let start = end.saturating_sub(height);
    let lines: Vec<Line> = app
        .logs
        .iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|entry| {
            Line::from(format!(
                "{} [{}] [{:?}] {}{}",
                entry.timestamp.format("%H:%M:%SZ"),
                safe_text(&entry.service),
                entry.level,
                safe_text(&entry.message),
                if entry.truncated { " [truncated]" } else { "" }
            ))
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().title(" Logs ").borders(Borders::ALL)),
        rows[2],
    );
    let message = if app.confirming_stop {
        "Stop the entire stack? Press s or Enter to confirm; Esc/n to cancel".to_string()
    } else if !app.notice.is_empty() {
        safe_text(&app.notice)
    } else {
        app.selected_name()
            .and_then(|name| {
                app.snapshot.services[&name]
                    .last_error
                    .as_deref()
                    .map(safe_text)
            })
            .unwrap_or_default()
    };
    frame.render_widget(
        Paragraph::new(format!(
            "{message}\nUp/Down select  r restart  s stop stack  PgUp/PgDn logs  q quit"
        )),
        rows[3],
    );
    if !app.color {
        for cell in &mut frame.buffer_mut().content {
            cell.set_fg(Color::Reset).set_bg(Color::Reset);
        }
    }
}

fn safe_text(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() && character != '\t' {
            safe.extend(character.escape_default());
        } else {
            safe.push(character);
        }
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::service_manager::ServiceSnapshot;
    use ratatui::{backend::TestBackend, Terminal};
    use std::collections::BTreeMap;

    fn app() -> App {
        let services = ["api", "worker"]
            .into_iter()
            .map(|name| (name.into(), ServiceSnapshot::default()))
            .collect::<BTreeMap<_, _>>();
        App::new(
            RuntimeSnapshot {
                supervisor_pid: 42,
                services,
            },
            Vec::new(),
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn test_top_confirmation_and_selection() {
        let mut app = app();
        assert!(matches!(app.key(key(KeyCode::Down)), Action::None));
        assert_eq!(app.selected_name().as_deref(), Some("worker"));
        app.update_snapshot(RuntimeSnapshot {
            supervisor_pid: 42,
            services: [("worker".into(), ServiceSnapshot::default())].into(),
        });
        assert_eq!(app.selected_name().as_deref(), Some("worker"));
        assert!(matches!(app.key(key(KeyCode::Char('s'))), Action::None));
        assert!(app.confirming_stop);
        assert!(matches!(app.key(key(KeyCode::Esc)), Action::None));
        assert!(!app.confirming_stop);
        app.key(key(KeyCode::Char('s')));
        assert!(matches!(app.key(key(KeyCode::Char('q'))), Action::Quit));
        app.key(key(KeyCode::Esc));
        assert!(
            matches!(app.key(key(KeyCode::Char('r'))), Action::Restart(name) if name == "worker")
        );
        assert!(app.busy);
        assert!(matches!(app.key(key(KeyCode::Char('s'))), Action::None));
        app.busy = false;
        app.key(key(KeyCode::Char('s')));
        assert!(matches!(app.key(key(KeyCode::Enter)), Action::Stop));
    }

    #[test]
    fn test_top_render_small_and_normal_terminal() {
        let app = app();
        for (width, height) in [(20, 5), (80, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &app)).unwrap();
        }
    }

    #[test]
    fn test_top_escapes_control_characters_in_service_output() {
        assert_eq!(safe_text("hello\x1b[2J\nworld"), "hello\\u{1b}[2J\\nworld");
    }
}
