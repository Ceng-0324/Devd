//! Bounded diagnostic reports from live or explicitly selected retained sources.
mod output;
mod stored;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::{
    instances::Identity,
    protocol::{self, Request, Response},
};
use crate::{
    core::{
        diagnostics::{self, ExplainReport},
        events::{
            query::{EventBatch, EventCursor, EventFilter, EventQuery, PersistenceState},
            EventData, EventHistory,
        },
        resource_monitor::ResourceUsage,
        service_manager::{RuntimeSnapshot, ServiceSnapshot, ServiceState},
    },
    logging::{LogEntry, LogHistory},
};
use output::{publish_report, CancelOnDrop, Control, DEADLINE};
use stored::Selection;

const EVENT_LIMIT: usize = 1000;
const LOG_LIMIT: usize = 200;
const SERVICE_LIMIT: usize = 256;

#[derive(clap::Args)]
pub(super) struct Args {
    /// New JSON filename; an existing file is never replaced.
    #[arg(long)]
    output: PathBuf,
    /// Include the most recent 200 raw application log entries.
    #[arg(long)]
    include_logs: bool,
    /// Read retained evidence without connecting to a supervisor.
    #[arg(long, requires = "run")]
    stored: bool,
    /// Retained run UUID or latest in file order (requires --stored).
    #[arg(long, requires = "stored", value_parser = stored::parse_run)]
    run: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Report {
    schema_version: u16,
    identity: Option<Identity>,
    #[serde(default = "live_source")]
    source: crate::core::events::query::EventSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    stored: Option<stored::Evidence>,
    captured_from: DateTime<Utc>,
    captured_to: DateTime<Utc>,
    /// A stable bracket is an observation, not a cross-source atomic snapshot.
    stable_during_capture: bool,
    event_watermark_after: u64,
    omitted_fields: Vec<String>,
    state: BTreeMap<String, SafeServiceState>,
    events: EventBatch,
    explanations: BTreeMap<String, ExplainReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    logs: Option<Vec<LogEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_tail_limit: Option<usize>,
}

/// Explicit allowlist: error strings, commands and environment values are absent.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct SafeServiceState {
    status: ServiceState,
    pid: Option<u32>,
    started_at: Option<DateTime<Utc>>,
    restart_count: u32,
    consecutive_failures: u32,
    last_exit_code: Option<i32>,
    last_exit_signal: Option<i32>,
    resources: Option<ResourceUsage>,
    event_generation: Option<u64>,
}

impl From<&ServiceSnapshot> for SafeServiceState {
    fn from(value: &ServiceSnapshot) -> Self {
        Self {
            status: value.status,
            pid: value.pid,
            started_at: value.started_at,
            restart_count: value.restart_count,
            consecutive_failures: value.consecutive_failures,
            last_exit_code: value.last_exit_code,
            last_exit_signal: value.last_exit_signal,
            resources: value.resources.clone(),
            event_generation: value.event_generation,
        }
    }
}

pub(super) fn capture(
    identity: Identity,
    snapshots: &watch::Receiver<RuntimeSnapshot>,
    history: &EventHistory,
    persistence: PersistenceState,
    logs: Option<&LogHistory>,
) -> Result<Report, String> {
    let captured_from = Utc::now();
    let before = snapshots.borrow().clone();
    let window = history.snapshot();
    let mut events = EventQuery {
        filter: EventFilter::default(),
        tail: EVENT_LIMIT,
        cursor: Some(EventCursor {
            run_id: window.context.run_id.clone(),
            next_sequence: 0,
        }),
    }
    .select(window)?;
    events.persistence = Some(persistence);
    sanitize_events(&mut events);
    let logs = logs.map(|history| {
        history
            .recent(None, LOG_LIMIT)
            .into_iter()
            .map(|entry| (*entry).clone())
            .collect()
    });
    let after = snapshots.borrow().clone();
    let event_watermark_after = history.snapshot().next_sequence;
    let captured_to = Utc::now();
    let event_run = events.context.as_ref().map(|c| c.run_id.as_str());
    if event_run != Some(identity.run_id.as_str())
        || after.event_run_id.as_deref() != Some(identity.run_id.as_str())
    {
        return Err("diagnostic sources disagree on supervisor run identity".into());
    }
    let stable_during_capture = before == after
        && events
            .cursor
            .as_ref()
            .is_some_and(|c| c.next_sequence == event_watermark_after);
    let mut names: BTreeSet<_> = after.services.keys().cloned().collect();
    names.extend(
        events
            .entries
            .iter()
            .filter_map(|event| event.service.clone()),
    );
    if names.len() > SERVICE_LIMIT {
        return Err(format!(
            "diagnostic export exceeds {SERVICE_LIMIT} service names"
        ));
    }
    let explanations = names
        .into_iter()
        .map(|name| {
            let explanation = diagnostics::explain(&name, Some(&after), &events);
            (name, explanation)
        })
        .collect();
    Ok(Report {
        schema_version: 1,
        identity: Some(identity),
        source: live_source(),
        stored: None,
        captured_from,
        captured_to,
        stable_during_capture,
        event_watermark_after,
        omitted_fields: omitted_fields(),
        state: after
            .services
            .iter()
            .map(|(name, state)| (name.clone(), state.into()))
            .collect(),
        events,
        explanations,
        log_tail_limit: logs.as_ref().map(|_| LOG_LIMIT),
        logs,
    })
}

fn sanitize_events(events: &mut EventBatch) {
    for event in &mut events.entries {
        sanitize_event(event);
    }
}

pub(super) fn sanitize_event(event: &mut crate::core::events::LifecycleEvent) {
    if let EventData::ReloadFinished { failure, .. } = &mut event.data {
        *failure = None;
    }
}

fn omitted_fields() -> Vec<String> {
    [
        "state.*.last_error",
        "state.*.resource_restart_reason",
        "events.entries[].data.reload-finished.failure",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn live_source() -> crate::core::events::query::EventSource {
    crate::core::events::query::EventSource::Live
}

pub(super) async fn run(
    args: Args,
    socket: &Path,
    config: &Path,
    state: &Path,
    profile: Option<&str>,
) -> Result<()> {
    let mut signals = crate::platform::shutdown::Shutdown::new(false)?;
    let control = Control::new();
    let _cancel_on_drop = CancelOnDrop(control.clone());
    let selected = Selection {
        requested_run: args.run.clone().unwrap_or_default(),
        config: config.to_owned(),
        state_dir: state.to_owned(),
        profile: profile.map(str::to_owned),
        selected_instance_id: String::new(),
        historical_instance_identity: "not-retained-in-event-files".into(),
    };
    let work_control = control.clone();
    let output = args.output.clone();
    let work = async {
        let report = if args.stored {
            let control = work_control.clone();
            tokio::task::spawn_blocking(move || {
                stored::capture(selected, args.include_logs, &control)
            })
            .await
            .context("offline export worker failed")??
        } else {
            let Response::Export(report) = protocol::request(
                socket,
                Request::Export {
                    include_logs: args.include_logs,
                },
            )
            .await?
            else {
                bail!("unexpected diagnostic export response");
            };
            *report
        };
        tokio::task::spawn_blocking(move || publish_report(&args.output, &report, &work_control))
            .await
            .context("export publication worker failed")?
    };
    tokio::pin!(work);
    tokio::select! {
        biased;
        _ = signals.recv() => {
            if control.cancel() { bail!("export cancelled before publication"); }
            work.await?;
        },
        _ = tokio::time::sleep(DEADLINE) => {
            if control.cancel() { bail!("export exceeded 30s deadline before publication"); }
            work.await?;
        },
        result = &mut work => result?,
    }
    // Publication has completed. Slow stdout must not hold this command forever.
    let text = format!("Saved diagnostic report to {output:?}\n");
    use tokio::io::AsyncWriteExt;
    let mut stdout = super::stdout::Stdout::new()?;
    tokio::select! {
        _ = signals.recv() => bail!("report saved; confirmation output cancelled"),
        result = tokio::time::timeout(protocol::IO_TIMEOUT, async { stdout.write_all(text.as_bytes()).await?; stdout.flush().await }) => result.context("report saved; confirmation output timed out")??,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        events::{EventRecorder, EVENT_CAPACITY},
        reload::ReloadOutcome,
    };

    #[test]
    fn test_export_omits_error_text_and_reports_retained_history_gaps() {
        let recorder = EventRecorder::new("/tmp/state".into(), None);
        let run_id = recorder.run_id().to_owned();
        for _ in 0..EVENT_CAPACITY + 3 {
            recorder.record(Some("api"), Some(1), None, EventData::Starting);
        }
        recorder.record(
            Some("api"),
            Some(1),
            None,
            EventData::ReloadFinished {
                plan_id: "plan".into(),
                outcome: ReloadOutcome::Failed,
                config_committed: false,
                stopped: vec![],
                started: vec![],
                ready: vec![],
                failure: Some("SECRET-ERROR-TEXT".into()),
            },
        );
        let snapshot = RuntimeSnapshot {
            supervisor_pid: 1,
            event_run_id: Some(run_id.clone()),
            services: BTreeMap::from([(
                "api".into(),
                ServiceSnapshot {
                    status: ServiceState::Failed,
                    last_error: Some("SECRET-ERROR-TEXT".into()),
                    resource_restart_reason: Some("SECRET-ERROR-TEXT".into()),
                    event_generation: Some(1),
                    ..Default::default()
                },
            )]),
        };
        let (tx, rx) = watch::channel(snapshot);
        let identity = Identity {
            schema_version: 1,
            instance_id: "instance".into(),
            run_id,
            supervisor_pid: 1,
            started_at: Utc::now(),
            config: "/tmp/devd.yml".into(),
            state_dir: "/tmp/state".into(),
            profile: None,
            project_root: "/tmp".into(),
            git: None,
        };
        let report = capture(
            identity,
            &rx,
            &recorder.history(),
            PersistenceState::Disabled,
            None,
        )
        .unwrap();
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("SECRET-ERROR-TEXT"));
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value["state"]["api"].get("last_error").is_none());
        assert!(value["state"]["api"]
            .get("resource_restart_reason")
            .is_none());
        assert!(report
            .omitted_fields
            .iter()
            .any(|field| field == "state.*.last_error"));
        assert!(matches!(
            report.events.entries.last().unwrap().data,
            EventData::ReloadFinished { failure: None, .. }
        ));
        assert!(report.stable_during_capture);
        assert_eq!(report.events.entries.len(), EVENT_LIMIT);
        assert!(report
            .events
            .gaps
            .iter()
            .any(|gap| matches!(gap, crate::core::events::query::EventGap::Retention { .. })));
        assert!(report.events.gaps.iter().any(|gap| matches!(
            gap,
            crate::core::events::query::EventGap::TailLimited { .. }
        )));
        assert!(!report.explanations["api"].complete);
        assert!(report.logs.is_none());
        let mut legacy = value.clone();
        legacy.as_object_mut().unwrap().remove("source");
        let decoded: Report = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            decoded.source,
            crate::core::events::query::EventSource::Live
        );
        assert!(decoded.identity.is_some() && decoded.stored.is_none());
        let mut wrong = report.identity.clone().unwrap();
        wrong.run_id = uuid::Uuid::new_v4().to_string();
        assert!(capture(
            wrong,
            &rx,
            &recorder.history(),
            PersistenceState::Disabled,
            None
        )
        .unwrap_err()
        .to_string()
        .contains("run identity"));
        let mut crowded = tx.borrow().clone();
        for index in 0..=SERVICE_LIMIT {
            crowded
                .services
                .insert(format!("service-{index}"), ServiceSnapshot::default());
        }
        tx.send_replace(crowded);
        assert!(capture(
            report.identity.unwrap(),
            &rx,
            &recorder.history(),
            PersistenceState::Disabled,
            None
        )
        .unwrap_err()
        .to_string()
        .contains("service names"));
    }
}
