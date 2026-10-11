//! Offline sources stay distinct from live identity, status and runless log files.
use super::{output::Control, Report, SafeServiceState, EVENT_LIMIT, LOG_LIMIT, SERVICE_LIMIT};
use crate::{
    core::{
        events::{query::EventSource, storage::read_run},
        service_manager::RuntimeSnapshot,
    },
    platform::files,
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read},
    path::{Path, PathBuf},
};

const MAX_STATE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Evidence {
    selection: Selection,
    begin_retained: bool,
    end_retained: bool,
    event_coverage_complete: bool,
    atomic: bool,
    live_state: String,
    historical_state: HistoricalState,
    #[serde(skip_serializing_if = "Option::is_none")]
    unassigned_logs: Option<LogAttachment>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Selection {
    pub(super) requested_run: String,
    pub(super) config: PathBuf,
    pub(super) state_dir: PathBuf,
    pub(super) profile: Option<String>,
    // Derived from command selection, never represented as historical identity.
    pub(super) selected_instance_id: String,
    pub(super) historical_instance_identity: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct HistoricalState {
    source: PathBuf,
    availability: String,
    stale: bool,
    observed_run_id: Option<String>,
    file_modified_at: Option<DateTime<Utc>>,
    state: Option<BTreeMap<String, SafeServiceState>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::cli) struct LogAttachment {
    source: PathBuf,
    availability: String,
    run_attribution: String,
    potentially_sensitive: bool,
    coverage: String,
    tail_limit: usize,
    records: Option<crate::logging::storage::StoredLogs>,
}

pub(super) fn parse_run(text: &str) -> Result<String, String> {
    if text == "latest" {
        Ok(text.into())
    } else {
        uuid::Uuid::parse_str(text)
            .map(|run| run.to_string())
            .map_err(|_| "--run must be a UUID or latest".into())
    }
}

pub(super) fn capture(
    mut selection: Selection,
    include_logs: bool,
    control: &Control,
) -> Result<Report> {
    let captured_from = Utc::now();
    control.check()?;
    let metadata = std::fs::symlink_metadata(&selection.state_dir)?;
    if !metadata.is_dir() || files::is_link(&metadata) {
        bail!("state directory is not a regular directory");
    }
    selection.selected_instance_id = super::super::instances::instance_id(
        &selection.config,
        &std::fs::canonicalize(&selection.state_dir)?,
        selection.profile.as_deref(),
    )?;
    // No create/open-write: offline reads must not fabricate a supervisor lease.
    let lock_file =
        files::open_regular(&selection.state_dir.join("services.json.lock"), false, true)
            .context("offline export requires an existing safe supervisor state lock")?;
    let _lease = files::FileLock::acquire(lock_file, true)
        .context("state is in use; stop the supervisor before offline export")?;
    let run = read_run(
        &selection.state_dir.join("events"),
        &selection.requested_run,
        EVENT_LIMIT,
        || control.check(),
    )
    .context("cannot read selected retained events; enable event persistence before running")?;
    let mut events = run.batch;
    super::sanitize_events(&mut events);
    let run_id = &events
        .context
        .as_ref()
        .context("missing stored event context")?
        .run_id;
    let historical_state = read_state(&selection.state_dir.join("services.json"), run_id, control)?;
    let unassigned_logs = if include_logs {
        Some(read_logs(&selection.state_dir.join("logs"), control)?)
    } else {
        None
    };
    let mut names: BTreeSet<_> = events
        .entries
        .iter()
        .filter_map(|event| event.service.clone())
        .collect();
    if let Some(state) = &historical_state.state {
        names.extend(state.keys().cloned());
    }
    if names.len() > SERVICE_LIMIT {
        bail!("diagnostic export exceeds {SERVICE_LIMIT} service names");
    }
    let event_coverage_complete = run.begin_retained
        && run.end_retained
        && events.gaps.is_empty()
        && events.omitted_gaps == 0
        && !events.entries.iter().any(|e| e.truncated);
    let explanations = names
        .into_iter()
        .map(|name| {
            let mut report = crate::core::diagnostics::explain(&name, None, &events);
            if report.complete && !event_coverage_complete {
                report.complete = false;
                report
                    .details
                    .push("所选运行的起止标记或事件内容未完整保留；解释仅基于已有证据".into());
            }
            (name, report)
        })
        .collect();
    let event_watermark_after = events
        .cursor
        .as_ref()
        .map_or(0, |cursor| cursor.next_sequence);
    control.check()?;
    Ok(Report {
        schema_version: 2,
        identity: None,
        source: EventSource::Stored,
        captured_from,
        captured_to: Utc::now(),
        stable_during_capture: false,
        event_watermark_after,
        omitted_fields: super::omitted_fields()
            .into_iter()
            .chain([
                "stored.historical_state.state.*.last_error".into(),
                "stored.historical_state.state.*.resource_restart_reason".into(),
            ])
            .collect(),
        state: BTreeMap::new(),
        events,
        explanations,
        logs: None,
        log_tail_limit: None,
        incident: None,
        stored: Some(Evidence {
            selection,
            begin_retained: run.begin_retained,
            end_retained: run.end_retained,
            event_coverage_complete,
            atomic: false,
            live_state: "unavailable-offline".into(),
            historical_state,
            unassigned_logs,
        }),
    })
}

fn read_state(path: &Path, run: &str, control: &Control) -> Result<HistoricalState> {
    control.check()?;
    let mut evidence = HistoricalState {
        source: path.into(),
        availability: "missing".into(),
        stale: true,
        observed_run_id: None,
        file_modified_at: None,
        state: None,
    };
    let file = match files::open_regular(path, false, true) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(evidence),
        Err(error) => return Err(error).context("unsafe or unreadable historical state"),
    };
    evidence.file_modified_at = file.metadata()?.modified().ok().map(DateTime::from);
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        bail!("historical state exceeds 1 MiB");
    }
    let snapshot: RuntimeSnapshot = match serde_json::from_slice(&bytes) {
        Ok(snapshot) => snapshot,
        Err(_) => {
            evidence.availability = "invalid".into();
            return Ok(evidence);
        }
    };
    if snapshot
        .event_run_id
        .as_deref()
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_err())
    {
        evidence.availability = "invalid".into();
        return Ok(evidence);
    }
    evidence.observed_run_id = snapshot.event_run_id.clone();
    evidence.availability = match snapshot.event_run_id.as_deref() {
        Some(id) if id == run => {
            if snapshot.services.len() > SERVICE_LIMIT {
                bail!("historical state exceeds {SERVICE_LIMIT} services");
            }
            evidence.state = Some(
                snapshot
                    .services
                    .iter()
                    .map(|(name, value)| (name.clone(), value.into()))
                    .collect(),
            );
            "retained-matching-run"
        }
        Some(_) => "different-run",
        None => "unknown-run",
    }
    .into();
    Ok(evidence)
}

pub(super) fn read_logs(path: &Path, control: &Control) -> Result<LogAttachment> {
    let records = match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
        Ok(_) => Some(crate::logging::storage::read_export(
            path,
            LOG_LIMIT,
            || control.check(),
        )?),
    };
    Ok(LogAttachment {
        source: path.into(),
        availability: if records.is_some() {
            "retained"
        } else {
            "missing"
        }
        .into(),
        run_attribution: "unknown-no-run-id-in-log-format".into(),
        potentially_sensitive: true,
        coverage: "unknown-rotation-and-run-boundaries-not-recorded".into(),
        tail_limit: LOG_LIMIT,
        records,
    })
}

#[cfg(test)]
mod tests {
    use super::super::output::{publish_report, MAX_REPORT_BYTES};
    use super::*;
    use crate::{
        core::events::{storage::EventStorage, EventData, EventRecorder},
        storage::StorageOptions,
    };

    fn fixture(root: &Path) -> Selection {
        std::fs::write(root.join("services.json.lock"), "").unwrap();
        let recorder = EventRecorder::new(root.join("services.json"), None);
        let storage = EventStorage::open(
            &root.join("events"),
            StorageOptions::default(),
            recorder.history().snapshot().context,
        )
        .unwrap();
        let receiver = recorder.subscribe();
        recorder.record(Some("api"), Some(1), None, EventData::Starting);
        drop(recorder);
        storage.run(receiver).unwrap();
        Selection {
            requested_run: "latest".into(),
            config: root.join("missing.yml"),
            state_dir: root.into(),
            profile: None,
            selected_instance_id: "selected-not-historical".into(),
            historical_instance_identity: "not-retained-in-event-files".into(),
        }
    }

    #[test]
    fn test_cancelled_or_expired_export_cannot_publish_and_releases_lease() {
        let root = tempfile::tempdir().unwrap();
        let selection = fixture(root.path());
        let report = capture(selection, false, &Control::new()).unwrap();
        let control = Control::new();
        assert!(control.cancel());
        let output = root.path().join("cancelled.json");
        assert!(publish_report(&output, &report, &control).is_err());
        assert!(!output.exists());
        let expired = Control::expired();
        assert!(publish_report(&output, &report, &expired).is_err());
        assert!(!output.exists());
        let file =
            files::open_regular(&root.path().join("services.json.lock"), false, true).unwrap();
        assert!(files::FileLock::acquire(file, false).is_ok());
        let control = Control::new();
        control.publish().unwrap();
        assert!(!control.cancel()); // publication has already been accepted
    }

    #[test]
    fn test_invalid_historical_run_and_free_text_are_not_exported() {
        let root = tempfile::tempdir().unwrap();
        let selection = fixture(root.path());
        let path = root.path().join("services.json");
        std::fs::write(
            &path,
            r#"{"supervisor_pid":1,"event_run_id":"SECRET-BAD-RUN","services":{}}"#,
        )
        .unwrap();
        let report = capture(selection, false, &Control::new()).unwrap();
        let raw = serde_json::to_string(&report).unwrap();
        assert!(!raw.contains("SECRET-BAD-RUN"));
        assert_eq!(
            report.stored.unwrap().historical_state.availability,
            "invalid"
        );
    }

    #[test]
    fn test_report_budget_and_existing_output_preserve_files() {
        let root = tempfile::tempdir().unwrap();
        let mut report = capture(fixture(root.path()), false, &Control::new()).unwrap();
        let output = root.path().join("report.json");
        std::fs::write(&output, "keep").unwrap();
        assert!(publish_report(&output, &report, &Control::new()).is_err());
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "keep");
        report.omitted_fields = vec!["x".repeat(MAX_REPORT_BYTES)];
        let absent = root.path().join("oversized.json");
        assert!(publish_report(&absent, &report, &Control::new())
            .unwrap_err()
            .to_string()
            .contains("encode"));
        assert!(!absent.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 3);
    }
}
