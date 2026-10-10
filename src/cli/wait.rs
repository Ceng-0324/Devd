use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncReadExt, sync::watch, time::Instant};

use super::{
    instances::Identity,
    protocol::{self, Request, Response},
    transport::Stream,
};
use crate::{
    config::{parse_duration, DevdConfig},
    core::{
        readiness::{self, ControlState, Observation, Outcome},
        service_manager::RuntimeSnapshot,
    },
};

#[derive(clap::Args)]
pub(super) struct Args {
    /// Exact service names; omitted means every service in the live configuration.
    services: Vec<String>,
    /// Total wait deadline, including connection setup (1ms-1h).
    #[arg(long, default_value = "30s", value_parser = timeout)]
    timeout: Duration,
    /// Print one versioned report, including unsuccessful outcomes.
    #[arg(long)]
    json: bool,
}

fn timeout(value: &str) -> Result<Duration, String> {
    let duration = parse_duration(value).map_err(|error| error.to_string())?;
    if !(Duration::from_millis(1)..=Duration::from_secs(3600)).contains(&duration) {
        return Err("--timeout must be between 1ms and 1h".into());
    }
    Ok(duration)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Report {
    pub schema_version: u16,
    pub instance_id: Option<String>,
    pub run_id: Option<String>,
    pub observed_at: Option<DateTime<Utc>>,
    pub elapsed_ms: u64,
    #[serde(flatten)]
    pub observation: Observation,
    pub message: Option<String>,
}

impl Report {
    fn empty(outcome: Outcome) -> Self {
        Self {
            schema_version: 1,
            instance_id: None,
            run_id: None,
            observed_at: None,
            elapsed_ms: 0,
            observation: Observation {
                outcome,
                services: BTreeMap::new(),
                blocking: vec![],
            },
            message: None,
        }
    }

    fn finish(&mut self, outcome: Outcome, message: impl Into<String>) {
        self.observation.outcome = outcome;
        self.message = Some(message.into());
    }
}

pub(super) async fn run(args: Args, socket: &Path) -> Result<()> {
    let mut signals = crate::platform::shutdown::Shutdown::new(false)?;
    let started = Instant::now();
    let mut report = Report::empty(Outcome::Unavailable);
    let message = Request::Wait {
        services: args.services,
        timeout_ms: args.timeout.as_millis() as u64,
    };
    // Requests must fit the existing control protocol's bounded frame.
    if serde_json::to_vec(&message)?.len() > protocol::MAX_REQUEST {
        bail!("wait service selection exceeds the 4096-byte control request limit");
    }
    tokio::select! {
        biased;
        _ = signals.recv() => report.finish(Outcome::Cancelled, "wait cancelled; services were not changed"),
        _ = tokio::time::sleep_until(started + args.timeout) => report.finish(Outcome::TimedOut, "readiness deadline reached; evidence is the last received observation"),
        result = receive(socket, message, &mut report) => {
            if let Err(error) = result {
                let outcome = if report.run_id.is_some() { Outcome::Disconnected } else { Outcome::Unavailable };
                report.finish(outcome, format!("{error:#}"));
            }
        }
    }
    report.elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    super::output(&render(&report, args.json)?)?;
    if report.observation.outcome != Outcome::Ready {
        bail!("readiness wait ended: {:?}", report.observation.outcome);
    }
    Ok(())
}

async fn receive(socket: &Path, message: Request, report: &mut Report) -> Result<()> {
    let mut stream = protocol::connect(socket).await?;
    protocol::write(&mut stream, &message).await?;
    receive_reports(&mut stream, report).await
}

async fn receive_reports(stream: &mut Stream, report: &mut Report) -> Result<()> {
    loop {
        let next = protocol::next_response(stream)
            .await?
            .context("supervisor closed readiness stream")?;
        match next {
            Response::Wait(next) => {
                if next.schema_version != 1 {
                    bail!("unsupported readiness report schema");
                }
                if next.run_id.is_none() || next.instance_id.is_none() {
                    bail!("supervisor omitted readiness identity");
                }
                if report.run_id.is_some()
                    && (report.run_id != next.run_id || report.instance_id != next.instance_id)
                {
                    bail!("supervisor identity changed during readiness wait");
                }
                *report = *next;
                if report.observation.outcome != Outcome::Waiting {
                    return Ok(());
                }
            }
            Response::Error(error) => bail!("{error}"),
            _ => bail!("unexpected readiness response; the supervisor may not support wait"),
        }
    }
}

fn render(report: &Report, json: bool) -> Result<String> {
    use std::fmt::Write;
    if json {
        return Ok(format!("{}\n", serde_json::to_string_pretty(report)?));
    }
    let mut text = format!(
        "Readiness: {:?} ({} ms)\n",
        report.observation.outcome, report.elapsed_ms
    );
    if let Some(run) = &report.run_id {
        writeln!(text, "Run: {run:?}")?;
    }
    for (name, evidence) in &report.observation.services {
        writeln!(
            text,
            "{name:?}: {} - state={:?}, PID={:?}, generation={:?}, requires={}, restart_pending={}",
            if evidence.ready { "ready" } else { "waiting" },
            evidence.status,
            evidence.pid,
            evidence.generation,
            if evidence.requires_healthy {
                "healthy"
            } else {
                "running"
            },
            evidence.restart_pending
        )?;
        if let Some(error) = &evidence.last_error {
            writeln!(text, "  Error: {error:?}")?;
        }
    }
    if let Some(message) = &report.message {
        writeln!(text, "Detail: {message:?}")?;
    }
    Ok(text)
}

pub(super) struct Source {
    pub identity: Identity,
    pub snapshots: watch::Receiver<RuntimeSnapshot>,
    pub configurations: watch::Receiver<Arc<DevdConfig>>,
    pub control: watch::Receiver<ControlState>,
    pub stopping: watch::Receiver<bool>,
}

pub(super) async fn serve(
    stream: &mut Stream,
    services: Vec<String>,
    timeout_ms: u64,
    mut source: Source,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Result<()> {
    let mut report = Report::empty(Outcome::Waiting);
    report.instance_id = Some(source.identity.instance_id);
    report.run_id = Some(source.identity.run_id);
    if permit.is_none() {
        report.finish(Outcome::Busy, "too many readiness waits (maximum 8)");
        return protocol::write(stream, &Response::Wait(Box::new(report))).await;
    }
    let _permit = permit;
    if !(1..=3_600_000).contains(&timeout_ms) {
        return protocol::write(
            stream,
            &Response::Error("wait timeout must be between 1ms and 1h".into()),
        )
        .await;
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let (selected, initial_control) = {
        // Same lock order as configuration commit and reload preview.
        let _snapshot = source.snapshots.borrow();
        let config = source.configurations.borrow();
        let selected = if services.is_empty() {
            config
                .services
                .iter()
                .map(|(name, config)| (name.clone(), config.healthcheck.is_some()))
                .collect()
        } else {
            services
                .into_iter()
                .map(|name| {
                    let probe = config
                        .services
                        .get(&name)
                        .is_some_and(|s| s.healthcheck.is_some());
                    (name, probe)
                })
                .collect()
        };
        (selected, source.control.borrow().clone())
    };
    let mut disconnected = [0];
    loop {
        let observation = {
            let snapshot = source.snapshots.borrow_and_update();
            let mut control = source.control.borrow_and_update().clone();
            control.reloading |= initial_control.reloading;
            control.stopping |= *source.stopping.borrow_and_update();
            readiness::observe(&selected, &snapshot, &control, initial_control.reload_epoch)
        };
        if report.observed_at.is_none() || report.observation != observation {
            report.observation = observation;
            report.observed_at = Some(Utc::now());
            tokio::time::timeout_at(
                deadline,
                protocol::write(stream, &Response::Wait(Box::new(report.clone()))),
            )
            .await??;
            if report.observation.outcome != Outcome::Waiting {
                return Ok(());
            }
        }
        tokio::select! {
            biased;
            _ = stream.read(&mut disconnected) => return Ok(()),
            _ = tokio::time::sleep_until(deadline) => {
                report.finish(Outcome::TimedOut, "readiness deadline reached");
                return protocol::write(stream, &Response::Wait(Box::new(report))).await;
            },
            result = source.stopping.changed() => { if result.is_err() { return Ok(()); } },
            result = source.control.changed() => { if result.is_err() { return Ok(()); } },
            result = source.snapshots.changed() => { if result.is_err() { return Ok(()); } },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::ConfigLoader,
        core::service_manager::{ServiceSnapshot, ServiceState},
    };
    use tokio::{sync::Semaphore, task::JoinHandle};

    struct Harness {
        snapshots: watch::Sender<RuntimeSnapshot>,
        configurations: watch::Sender<Arc<DevdConfig>>,
        control: watch::Sender<ControlState>,
        stopping: watch::Sender<bool>,
        waiters: Arc<Semaphore>,
    }

    impl Harness {
        fn new() -> Self {
            let config = ConfigLoader::from_str("version: '1'\nservices:\n  api:\n    command: ignored\n    healthcheck:\n      type: tcp\n      port: 12345\n", Path::new("devd.yml")).unwrap();
            Self {
                snapshots: watch::channel(RuntimeSnapshot {
                    supervisor_pid: 1,
                    event_run_id: Some("run".into()),
                    services: BTreeMap::from([(
                        "api".into(),
                        ServiceSnapshot {
                            status: ServiceState::Running,
                            pid: Some(42),
                            event_generation: Some(1),
                            ..Default::default()
                        },
                    )]),
                })
                .0,
                configurations: watch::channel(Arc::new(config)).0,
                control: watch::channel(ControlState::default()).0,
                stopping: watch::channel(false).0,
                waiters: Arc::new(Semaphore::new(8)),
            }
        }

        fn start(
            &self,
            services: Vec<String>,
            timeout_ms: u64,
        ) -> (Stream, JoinHandle<Result<()>>) {
            let (server, client) = tokio::io::duplex(8192);
            let source = Source {
                identity: Identity {
                    schema_version: 1,
                    instance_id: "instance".into(),
                    run_id: "run".into(),
                    supervisor_pid: 1,
                    started_at: Utc::now(),
                    config: "devd.yml".into(),
                    state_dir: ".devd".into(),
                    profile: None,
                    project_root: ".".into(),
                    git: None,
                },
                snapshots: self.snapshots.subscribe(),
                configurations: self.configurations.subscribe(),
                control: self.control.subscribe(),
                stopping: self.stopping.subscribe(),
            };
            let permit = self.waiters.clone().try_acquire_owned().ok();
            (
                Box::new(client),
                tokio::spawn(async move {
                    serve(
                        &mut (Box::new(server) as Stream),
                        services,
                        timeout_ms,
                        source,
                        permit,
                    )
                    .await
                }),
            )
        }
    }

    async fn report(stream: &mut Stream) -> Report {
        match tokio::time::timeout(Duration::from_secs(2), protocol::next_response(stream))
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        {
            Response::Wait(report) => *report,
            _ => panic!("expected readiness report"),
        }
    }

    #[tokio::test]
    async fn test_wait_stream_restart_and_completed_reload() {
        let h = Harness::new();
        let (mut stream, task) = h.start(vec![], 30_000);
        let initial = report(&mut stream).await;
        assert_eq!(initial.observation.outcome, Outcome::Waiting);
        assert_eq!(initial.run_id.as_deref(), Some("run"));
        h.control.send_modify(|s| {
            s.restarting.insert("api".into());
        });
        h.snapshots
            .send_modify(|s| s.services.get_mut("api").unwrap().status = ServiceState::Healthy);
        let pending = report(&mut stream).await;
        assert_eq!(pending.observation.outcome, Outcome::Waiting);
        assert!(pending.observation.services["api"].restart_pending);
        h.snapshots.send_modify(|s| {
            let state = s.services.get_mut("api").unwrap();
            state.pid = Some(43);
            state.event_generation = Some(2);
        });
        h.control.send_modify(|s| s.restarting.clear());
        let ready = report(&mut stream).await;
        assert_eq!(ready.observation.outcome, Outcome::Ready);
        assert_eq!(ready.observation.services["api"].generation, Some(2));
        task.await.unwrap().unwrap();

        h.snapshots
            .send_modify(|s| s.services.get_mut("api").unwrap().status = ServiceState::Running);
        let (mut stream, task) = h.start(vec![], 30_000);
        assert_eq!(
            report(&mut stream).await.observation.outcome,
            Outcome::Waiting
        );
        // The receiver may coalesce both writes: epoch must preserve the barrier.
        h.control.send_modify(|s| {
            s.reload_epoch += 1;
            s.reloading = true;
        });
        h.control.send_modify(|s| s.reloading = false);
        assert_eq!(
            report(&mut stream).await.observation.outcome,
            Outcome::Reloaded
        );
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn test_wait_stream_timeout_stop_and_unknown_service() {
        let h = Harness::new();
        let (mut stream, task) = h.start(vec![], 10);
        assert_eq!(
            report(&mut stream).await.observation.outcome,
            Outcome::Waiting
        );
        let timed_out = report(&mut stream).await;
        assert_eq!(timed_out.observation.outcome, Outcome::TimedOut);
        assert_eq!(timed_out.observation.blocking, ["api"]);
        task.await.unwrap().unwrap();
        let (mut stream, task) = h.start(vec!["unknown".into()], 30_000);
        assert_eq!(
            report(&mut stream).await.observation.outcome,
            Outcome::InvalidService
        );
        task.await.unwrap().unwrap();
        let (mut stream, task) = h.start(vec![], 30_000);
        assert_eq!(
            report(&mut stream).await.observation.outcome,
            Outcome::Waiting
        );
        h.stopping.send_replace(true);
        assert_eq!(
            report(&mut stream).await.observation.outcome,
            Outcome::Stopping
        );
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_wait_stream_capacity_disconnect_release_and_active_reload() {
        let h = Harness::new();
        let mut waiting = Vec::new();
        for _ in 0..8 {
            let (mut stream, task) = h.start(vec![], 30_000);
            assert_eq!(
                report(&mut stream).await.observation.outcome,
                Outcome::Waiting
            );
            waiting.push((stream, task));
        }
        let (mut stream, task) = h.start(vec![], 30_000);
        assert_eq!(report(&mut stream).await.observation.outcome, Outcome::Busy);
        task.await.unwrap().unwrap();
        for (stream, task) in waiting {
            drop(stream);
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        assert_eq!(h.waiters.available_permits(), 8);
        h.control.send_modify(|s| s.reloading = true);
        let (mut stream, task) = h.start(vec![], 30_000);
        assert_eq!(
            report(&mut stream).await.observation.outcome,
            Outcome::Reloaded
        );
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_wait_client_preserves_last_evidence_on_disconnect_and_rejects_new_run() {
        for new_run in [false, true] {
            let (server, client) = tokio::io::duplex(8192);
            let mut server: Stream = Box::new(server);
            let mut client: Stream = Box::new(client);
            let mut first = Report::empty(Outcome::Waiting);
            first.instance_id = Some("instance".into());
            first.run_id = Some("run".into());
            first.observation.blocking.push("api".into());
            protocol::write(&mut server, &Response::Wait(Box::new(first.clone())))
                .await
                .unwrap();
            if new_run {
                let mut second = first.clone();
                second.run_id = Some("another".into());
                second.observation.outcome = Outcome::Ready;
                protocol::write(&mut server, &Response::Wait(Box::new(second)))
                    .await
                    .unwrap();
            }
            drop(server);
            let mut received = Report::empty(Outcome::Unavailable);
            let error = receive_reports(&mut client, &mut received)
                .await
                .unwrap_err();
            assert!(error.to_string().contains(if new_run {
                "identity changed"
            } else {
                "closed readiness stream"
            }));
            assert_eq!(received.run_id, first.run_id);
            assert_eq!(received.observation.blocking, ["api"]);
        }
    }

    #[tokio::test]
    async fn test_wait_client_rejects_missing_identity_and_unknown_schema() {
        for unsupported_schema in [false, true] {
            let (server, client) = tokio::io::duplex(8192);
            let mut server: Stream = Box::new(server);
            let mut client: Stream = Box::new(client);
            let mut response = Report::empty(Outcome::Ready);
            if unsupported_schema {
                response.schema_version = 2;
            }
            protocol::write(&mut server, &Response::Wait(Box::new(response)))
                .await
                .unwrap();
            let mut received = Report::empty(Outcome::Unavailable);
            let error = receive_reports(&mut client, &mut received)
                .await
                .unwrap_err();
            assert!(error.to_string().contains(if unsupported_schema {
                "unsupported"
            } else {
                "omitted readiness identity"
            }));
            assert_eq!(received.observation.outcome, Outcome::Unavailable);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_wait_silent_stream_can_exceed_normal_request_deadline() {
        let h = Harness::new();
        let (mut stream, task) = h.start(vec![], 120_000);
        assert_eq!(
            report(&mut stream).await.observation.outcome,
            Outcome::Waiting
        );
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(!task.is_finished());
        h.snapshots
            .send_modify(|s| s.services.get_mut("api").unwrap().status = ServiceState::Healthy);
        assert_eq!(
            report(&mut stream).await.observation.outcome,
            Outcome::Ready
        );
        task.await.unwrap().unwrap();
    }

    #[test]
    fn test_wait_timeout_bounds_and_terminal_safe_text() {
        for invalid in ["0s", "0ms", "2h", "invalid"] {
            assert!(timeout(invalid).is_err());
        }
        for valid in ["1ms", "30s", "1h"] {
            assert!(timeout(valid).is_ok());
        }
        let mut report = Report::empty(Outcome::Failed);
        report.message = Some("\x1b[2J\nforged".into());
        assert!(!render(&report, false).unwrap().contains('\x1b'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&render(&report, true).unwrap()).unwrap()
                ["outcome"],
            "failed"
        );
    }
}
