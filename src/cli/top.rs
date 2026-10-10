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
use tokio::{sync::mpsc, task::JoinSet, time};

use super::{
    instances::Identity,
    protocol::{self, Request, Response},
};
use crate::{
    core::{
        events::query::{EventBatch, EventCursor, EventFilter, EventQuery},
        service_manager::{RuntimeSnapshot, ServiceState},
    },
    logging::{LogEntry, LogFilter},
};

mod timeline;
use timeline::Timeline;

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
    identity: Identity,
    snapshot: RuntimeSnapshot,
    selected: usize,
    logs: VecDeque<LogEntry>,
    log_offset: usize,
    timeline: Timeline,
    events_visible: bool,
    horizontal_offset: u16,
    page_size: usize,
    confirming_stop: bool,
    busy: bool,
    stopping: bool,
    notice: String,
    color: bool,
}

impl App {
    fn new(
        identity: Identity,
        snapshot: RuntimeSnapshot,
        entries: Vec<LogEntry>,
        timeline: Timeline,
    ) -> Self {
        let mut app = Self {
            identity,
            snapshot,
            selected: 0,
            logs: VecDeque::new(),
            log_offset: 0,
            timeline,
            events_visible: false,
            horizontal_offset: 0,
            page_size: 10,
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
                if self.events_visible {
                    self.timeline.offset = self
                        .timeline
                        .offset
                        .saturating_add(self.page_size)
                        .min(self.timeline.entries.len().saturating_sub(1));
                } else {
                    self.log_offset = self
                        .log_offset
                        .saturating_add(self.page_size)
                        .min(self.logs.len().saturating_sub(1));
                }
                Action::None
            }
            KeyCode::PageDown => {
                if self.events_visible {
                    self.timeline.offset = self.timeline.offset.saturating_sub(self.page_size);
                } else {
                    self.log_offset = self.log_offset.saturating_sub(self.page_size);
                }
                Action::None
            }
            KeyCode::End => {
                if self.events_visible {
                    self.timeline.offset = 0;
                } else {
                    self.log_offset = 0;
                }
                Action::None
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.events_visible = !self.events_visible;
                self.horizontal_offset = 0;
                Action::None
            }
            KeyCode::Left => {
                self.horizontal_offset = self.horizontal_offset.saturating_sub(20);
                Action::None
            }
            KeyCode::Right => {
                self.horizontal_offset = self.horizontal_offset.saturating_add(20);
                Action::None
            }
            KeyCode::Home => {
                self.horizontal_offset = 0;
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
    let mut signals = crate::platform::shutdown::Shutdown::new(true)?;
    tokio::select! {
        result = run_view(socket, color) => result,
        _ = signals.recv() => Ok(()),
    }
}

async fn query(socket: &Path, request: Request) -> Result<Response> {
    time::timeout(protocol::IO_TIMEOUT, protocol::request(socket, request))
        .await
        .context("top query timed out")?
}

async fn status(socket: &Path, identity: &Identity) -> Result<RuntimeSnapshot> {
    match query(socket, Request::Status).await? {
        Response::Status(snapshot) => {
            if snapshot.event_run_id.as_deref() != Some(identity.run_id.as_str())
                || snapshot.supervisor_pid != identity.supervisor_pid
            {
                bail!("supervisor run changed; reopen top to inspect the new run");
            }
            Ok(snapshot)
        }
        _ => bail!("unexpected status response"),
    }
}

async fn event_batch(socket: &Path, cursor: EventCursor) -> Result<EventBatch> {
    match query(
        socket,
        Request::Events {
            query: EventQuery {
                filter: EventFilter::default(),
                tail: timeline::LIMIT,
                cursor: Some(cursor),
            },
        },
    )
    .await?
    {
        Response::Events(batch) => Ok(batch),
        _ => bail!("unexpected events response"),
    }
}

async fn run_view(socket: &Path, color: bool) -> Result<()> {
    let Response::Identity(identity) = query(socket, Request::Identity).await? else {
        bail!("unexpected identity response");
    };
    let snapshot = status(socket, &identity).await?;
    let mut timeline = Timeline::new(identity.run_id.clone());
    timeline.ingest(event_batch(socket, timeline.cursor.clone()).await?)?;
    let (mut logs, entries) = follow(socket, &identity.run_id).await?;
    let mut app = App::new(*identity.clone(), snapshot, entries, timeline);
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
    let mut cursor = app.timeline.cursor.clone();
    readers.spawn(async move {
        loop {
            time::sleep(REFRESH).await;
            let snapshot = status(&status_socket, &identity).await?;
            let batch = event_batch(&status_socket, cursor.clone()).await?;
            cursor = batch.cursor.clone().context("event query has no cursor")?;
            if status_tx.send((snapshot, batch)).await.is_err() {
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
                if dirty { terminal.draw(|frame| render(frame, &mut app))?; dirty = false; }
                continue;
            }
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) => match app.key(key) {
                    Action::None => {},
                    Action::Quit => return Ok(()),
                    Action::Restart(name) => {
                        let socket = socket.to_path_buf();
                        let expected_run_id = Some(app.identity.run_id.clone());
                        controls.spawn(async move {
                            let result = protocol::request(&socket, Request::Restart { service: name.clone(), expected_run_id }).await;
                            ControlResult::Restart(name, result)
                        });
                    }
                    Action::Stop => {
                        app.stopping = true;
                        app.notice = "Stopping stack...".into();
                        let socket = socket.to_path_buf();
                        let expected_run_id = Some(app.identity.run_id.clone());
                        controls.spawn(async move { ControlResult::Stop(protocol::request(&socket, Request::Stop { expected_run_id }).await) });
                    }
                },
                Some(Ok(_)) => {},
                Some(Err(error)) => return Err(error).context("terminal input failed"),
                None => bail!("terminal input ended"),
            },
            Some((snapshot, batch)) = status_rx.recv() => {
                app.timeline.ingest(batch)?;
                app.update_snapshot(snapshot);
            },
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

async fn follow(socket: &Path, run_id: &str) -> Result<(super::transport::Stream, Vec<LogEntry>)> {
    let mut stream = protocol::connect(socket).await?;
    protocol::write(
        &mut stream,
        &Request::FollowLogs {
            service: None,
            tail: 100,
            filter: LogFilter::default(),
            expected_run_id: Some(run_id.into()),
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

fn render(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();
    if area.width < 60 || area.height < 16 {
        frame.render_widget(Paragraph::new("Terminal too small (minimum 60x16)"), area);
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Percentage(30),
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
            "  supervisor {}  |  {} services | profile {}",
            app.snapshot.supervisor_pid,
            app.snapshot.services.len(),
            safe_text(app.identity.profile.as_deref().unwrap_or("<base>")),
        )),
    ]);
    frame.render_widget(
        Paragraph::new(vec![
            title,
            Line::from(format!("instance {}", safe_text(&app.identity.instance_id))),
            Line::from(format!("run {}", safe_text(&app.identity.run_id))),
            Line::from(format!(
                "config {}",
                safe_text(&app.identity.config.to_string_lossy())
            )),
        ])
        .scroll((0, app.horizontal_offset)),
        rows[0],
    );
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
    if app.events_visible {
        let event_area = Layout::vertical([
            Constraint::Length(1 + u16::from(app.timeline.latest_gap.is_some())),
            Constraint::Min(0),
        ])
        .split(rows[2]);
        let mut summary = vec![Line::from(app.timeline.summary())];
        if let Some(gap) = &app.timeline.latest_gap {
            summary.push(Line::from(format!("Latest GAP: {}", safe_text(gap))));
        }
        frame.render_widget(
            Paragraph::new(summary).scroll((0, app.horizontal_offset)),
            event_area[0],
        );
        let height = event_area[1].height.saturating_sub(2) as usize;
        app.page_size = height.max(1);
        let end = app
            .timeline
            .entries
            .len()
            .saturating_sub(app.timeline.offset);
        let start = end.saturating_sub(height);
        let lines: Vec<Line> = app
            .timeline
            .entries
            .iter()
            .skip(start)
            .take(end - start)
            .map(|entry| Line::from(app.timeline.line(entry)))
            .collect();
        frame.render_widget(
            Paragraph::new(lines)
                .scroll((0, app.horizontal_offset))
                .block(
                    Block::default()
                        .title(" Events [Tab: Logs] ")
                        .borders(Borders::ALL),
                ),
            event_area[1],
        );
    } else {
        let height = rows[2].height.saturating_sub(2) as usize;
        app.page_size = height.max(1);
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
            Paragraph::new(lines)
                .scroll((0, app.horizontal_offset))
                .block(
                    Block::default()
                        .title(" Logs [Tab: Events] ")
                        .borders(Borders::ALL),
                ),
            rows[2],
        );
    }
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
            "{message}\nq quit  r restart  s stop  Tab view  PgUp/PgDn scroll  End live  arrows navigate"
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
            Identity {
                schema_version: 1,
                instance_id: "test-instance".into(),
                run_id: "test-run".into(),
                supervisor_pid: 42,
                started_at: chrono::Utc::now(),
                config: "/project/devd.yml".into(),
                state_dir: "/project/state".into(),
                profile: None,
                project_root: "/project".into(),
                git: None,
            },
            RuntimeSnapshot {
                supervisor_pid: 42,
                event_run_id: None,
                services,
            },
            Vec::new(),
            Timeline::new("test-run".into()),
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
            event_run_id: None,
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
        let mut app = app();
        app.color = false;
        for (width, height) in [(20, 5), (60, 16), (80, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            for visible in [false, true] {
                app.events_visible = visible;
                terminal.draw(|frame| render(frame, &mut app)).unwrap();
                let buffer = terminal.backend().buffer();
                let screen: String = buffer.content.iter().map(|c| c.symbol()).collect();
                if width >= 60 {
                    assert!(screen.contains("test-instance"));
                    assert!(screen.contains("test-run"));
                    assert!(screen.contains(if visible { "Events" } else { "Logs" }));
                }
                assert!(buffer
                    .content
                    .iter()
                    .all(|c| c.fg == Color::Reset && c.bg == Color::Reset));
            }
        }
    }

    #[test]
    fn test_top_tabs_scroll_independently_and_preserve_controls() {
        let mut app = app();
        app.log_offset = 20;
        app.timeline.offset = 30;
        app.key(key(KeyCode::Tab));
        assert!(app.events_visible);
        app.key(key(KeyCode::PageDown));
        assert_eq!(app.timeline.offset, 20);
        assert_eq!(app.log_offset, 20);
        app.key(key(KeyCode::Right));
        assert_eq!(app.horizontal_offset, 20);
        app.key(key(KeyCode::Home));
        assert_eq!(app.horizontal_offset, 0);
        app.key(key(KeyCode::End));
        assert_eq!(app.timeline.offset, 0);
        app.key(key(KeyCode::Char('s')));
        app.key(key(KeyCode::Tab));
        assert!(app.events_visible, "confirmation captures non-control keys");
        app.key(key(KeyCode::Esc));
        app.key(key(KeyCode::BackTab));
        assert!(!app.events_visible);
        assert_eq!(app.log_offset, 20);
        assert!(matches!(
            app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        ));
    }

    #[test]
    fn test_top_gap_banner_leaves_event_rows_visible_in_small_terminal() {
        use crate::core::events::{EventData, EventRecorder};
        let recorder = EventRecorder::new("state.json".into(), None);
        for _ in 0..5 {
            recorder.record(None, None, None, EventData::SupervisorStarted);
        }
        let mut app = app();
        app.timeline = Timeline::new(recorder.run_id().into());
        let batch = EventQuery {
            filter: EventFilter::default(),
            tail: 2,
            cursor: Some(app.timeline.cursor.clone()),
        }
        .select(recorder.history().snapshot())
        .unwrap();
        app.timeline.ingest(batch).unwrap();
        app.events_visible = true;
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("Gaps: 1"));
        assert!(screen.contains("Latest GAP:"));
        assert!(screen.contains("#4 g=- [supervisor] cause=-"));
        assert!(app.page_size > 0 && app.page_size <= 2);
    }

    #[test]
    fn test_top_escapes_control_characters_in_service_output() {
        assert_eq!(safe_text("hello\x1b[2J\nworld"), "hello\\u{1b}[2J\\nworld");
    }

    // Exercise the same query/control code on Unix sockets and Windows named
    // pipes. Console rendering itself is tested with TestBackend and Unix PTYs.
    #[tokio::test]
    async fn test_top_native_queries_and_controls_are_bound_to_one_run() {
        use crate::{
            config::DevdConfig,
            core::service_manager::{ManagerOptions, ServiceManager},
            logging::LogFormatter,
        };

        #[cfg(unix)]
        let root = tempfile::Builder::new()
            .prefix("dt-")
            .tempdir_in("/tmp")
            .unwrap();
        #[cfg(windows)]
        let root = tempfile::tempdir().unwrap();
        let command = format!(
            "{} --ignored --exact cli::top::tests::test_top_worker --nocapture",
            shell_words::quote(std::env::current_exe().unwrap().to_str().unwrap())
        );
        let config: DevdConfig = serde_yaml::from_str(&serde_yaml::to_string(&serde_json::json!({
            "version": "1", "services": {"worker": {"command": command, "restart": {"policy": "never"}}}
        })).unwrap()).unwrap();
        let config_path = root.path().join("devd.yml");
        tokio::fs::write(&config_path, serde_yaml::to_string(&config).unwrap())
            .await
            .unwrap();
        let socket = root.path().join("state/control.sock");
        let mut previous_run = "not-the-active-run".to_string();
        let mut previous_identity = None;
        for _ in 0..2 {
            let options = ManagerOptions::new(root.path().join("state/state.json"));
            let manager = ServiceManager::new(config.clone(), options.clone()).unwrap();
            let mut servers = JoinSet::new();
            servers.spawn(crate::cli::server::start(
                manager,
                options,
                socket.clone(),
                LogFormatter::default(),
                None,
                None,
                config_path.clone(),
            ));
            let identity = time::timeout(Duration::from_secs(15), async {
                loop {
                    if let Ok(Response::Identity(identity)) =
                        query(&socket, Request::Identity).await
                    {
                        if status(&socket, &identity).await.unwrap().services["worker"]
                            .pid
                            .is_some()
                        {
                            break identity;
                        }
                    }
                    assert!(
                        servers.try_join_next().is_none(),
                        "supervisor exited before readiness"
                    );
                    time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            assert_ne!(identity.run_id, previous_run);
            if let Some(old) = &previous_identity {
                assert!(status(&socket, old)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("run changed"));
            }
            let before = status(&socket, &identity).await.unwrap();
            for request in [
                Request::Stop {
                    expected_run_id: Some(previous_run.clone()),
                },
                Request::Restart {
                    service: "worker".into(),
                    expected_run_id: Some(previous_run.clone()),
                },
            ] {
                assert!(query(&socket, request)
                    .await
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("run changed"));
            }
            assert!(follow(&socket, &previous_run)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("run changed"));
            let after = status(&socket, &identity).await.unwrap();
            assert_eq!(before.services["worker"].pid, after.services["worker"].pid);
            assert_eq!(after.services["worker"].restart_count, 0);
            let (stream, _) = follow(&socket, &identity.run_id).await.unwrap();
            drop(stream); // cancelling the view leaves the supervisor running
            assert!(matches!(
                protocol::request(
                    &socket,
                    Request::Restart {
                        service: "worker".into(),
                        expected_run_id: Some(identity.run_id.clone()),
                    }
                )
                .await
                .unwrap(),
                Response::Restarted(_)
            ));
            let mut timeline = Timeline::new(identity.run_id.clone());
            timeline
                .ingest(event_batch(&socket, timeline.cursor.clone()).await.unwrap())
                .unwrap();
            assert!(timeline
                .entries
                .iter()
                .any(|entry| timeline.line(entry).contains("manual-restart-requested")));
            assert_eq!(
                status(&socket, &identity).await.unwrap().services["worker"].restart_count,
                1
            );
            assert!(matches!(
                query(
                    &socket,
                    Request::Stop {
                        expected_run_id: Some(identity.run_id.clone())
                    }
                )
                .await
                .unwrap(),
                Response::Stopping
            ));
            time::timeout(Duration::from_secs(10), servers.join_next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .unwrap();
            previous_run = identity.run_id.clone();
            previous_identity = Some(*identity);
        }
    }

    #[test]
    #[ignore = "native TUI protocol subprocess fixture"]
    fn test_top_worker() {
        println!("top fixture ready");
        std::thread::sleep(Duration::from_secs(60));
    }
}
