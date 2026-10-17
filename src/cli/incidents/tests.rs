use super::*;
use crate::core::{
    events::{EventRecorder, ProcessEvidence},
    state_store::StateStore,
};
use serde_json::Value;

fn event(sequence: u64, data: EventData) -> LifecycleEvent {
    LifecycleEvent {
        schema_version: 1,
        run_id: uuid::Uuid::nil().to_string(),
        sequence,
        timestamp: Utc::now(),
        elapsed: Duration::ZERO,
        service: Some("api".into()),
        generation: Some(0),
        cause: None,
        truncated: false,
        data,
    }
}

#[test]
fn test_incident_triggers_exclude_control_and_success() {
    for data in [
        EventData::Exited {
            code: Some(0),
            signal: None,
        },
        EventData::ManualRestartRequested,
        EventData::ServiceStopRequested {
            manual_restart: false,
        },
        EventData::HealthChanged {
            state: ServiceState::Healthy,
            consecutive_failures: 0,
            failure: None,
        },
        EventData::Starting,
    ] {
        assert!(!eligible(&data, false));
    }
    let exited = EventData::Exited {
        code: None,
        signal: Some(15),
    };
    assert!(eligible(&exited, false));
    assert!(!eligible(&exited, true));
    assert!(eligible(
        &EventData::SpawnFailed {
            failure: ProcessEvidence::Command
        },
        true
    ));
    assert!(eligible(
        &EventData::StateChanged {
            from: ServiceState::Pending,
            to: ServiceState::Failed
        },
        false
    ));
}

#[test]
fn test_incident_tail_bounds_and_free_text_omission() {
    let mut tail = Tail::default();
    for sequence in 0..205 {
        tail.push(event(sequence, EventData::Starting));
    }
    assert_eq!(tail.entries.len(), 200);
    assert_eq!(tail.omitted, 5);
    tail.push(event(
        205,
        EventData::ReloadFinished {
            plan_id: "p".into(),
            outcome: crate::core::reload::ReloadOutcome::Failed,
            config_committed: false,
            stopped: vec![],
            started: vec![],
            ready: vec![],
            failure: Some("SECRET".into()),
        },
    ));
    assert!(!serde_json::to_string(&tail.events())
        .unwrap()
        .contains("SECRET"));
    for sequence in 206..250 {
        tail.push(event(
            sequence,
            EventData::DependencyFailed {
                service: "x".repeat(16000),
                state: ServiceState::Failed,
            },
        ));
    }
    assert!(tail.bytes <= EVENT_BYTES);
    assert!(tail.entries.len() < 200 && tail.omitted > 6);
}

fn identity(root: &Path, run: &str) -> Identity {
    Identity {
        schema_version: 1,
        instance_id: "instance".into(),
        run_id: run.into(),
        supervisor_pid: 1,
        started_at: Utc::now(),
        config: root.join("devd.yml"),
        state_dir: root.into(),
        profile: None,
        project_root: root.into(),
        git: None,
    }
}
use std::path::Path;

fn records(root: &Path) -> Vec<Vec<Value>> {
    std::fs::read_dir(root.join("incidents"))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension().is_some_and(|ext| ext == "jsonl")).then(|| {
                let bytes = std::fs::read_to_string(path).unwrap();
                assert!(bytes.len() <= storage::MAX_BYTES);
                bytes
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect()
            })
        })
        .collect()
}

#[tokio::test]
async fn test_incident_correlation_generations_reload_privacy_and_slot_ownership() {
    let root = tempfile::tempdir().unwrap();
    let lease = Arc::new(
        StateStore::open(&root.path().join("state.json"))
            .await
            .unwrap(),
    );
    let storage = Storage::open(root.path(), lease).unwrap();
    let recorder = EventRecorder::new(root.path().join("state.json"), None);
    let (_, snapshots) = watch::channel(RuntimeSnapshot {
        supervisor_pid: 1,
        event_run_id: Some(recorder.run_id().into()),
        services: Default::default(),
    });
    let config: DevdConfig = serde_yaml::from_str("version: '1'\nservices:\n  api:\n    command: SECRET-COMMAND\n    env: {TOKEN: SECRET-ENV}\n").unwrap();
    let (_, configurations) = watch::channel(Arc::new(config));
    let (statistics, observed_statistics) = watch::channel(Statistics::default());
    let source = Source {
        identity: identity(root.path(), recorder.run_id()),
        events: recorder.subscribe(),
        snapshots,
        configurations,
        statistics,
    };
    let run = tokio::spawn(run(storage, source));
    let generation = recorder.record(Some("api"), None, None, EventData::GenerationPending);
    for _ in 0..3 {
        recorder.record(
            Some("api"),
            Some(generation),
            None,
            EventData::SpawnFailed {
                failure: ProcessEvidence::Command,
            },
        );
    }
    recorder.record(Some("api"), None, None, EventData::GenerationPending);
    recorder.record(
        Some("api"),
        Some(5),
        None,
        EventData::SpawnFailed {
            failure: ProcessEvidence::Command,
        },
    );
    recorder.record(
        None,
        None,
        None,
        EventData::ReloadStarted {
            plan_id: "plan".into(),
            base_config_id: "base".into(),
            candidate_config_id: "next".into(),
        },
    );
    drop(recorder);
    let stats = run.await.unwrap().unwrap();
    assert_eq!(*observed_statistics.borrow(), stats);
    assert_eq!(stats.admitted, 2);
    assert_eq!(stats.merged, 2);
    let records = records(root.path());
    assert_eq!(records.len(), 2);
    let all = serde_json::to_string(&records).unwrap();
    assert!(!all.contains("SECRET"));
    let reasons: Vec<_> = records
        .iter()
        .map(|r| r.last().unwrap()["data"]["reason"].as_str().unwrap())
        .collect();
    assert!(reasons.contains(&"generation-replaced") && reasons.contains(&"reload"));
    assert!(records
        .iter()
        .all(|r| r[0]["data"]["configuration"]["trigger_time_equivalence"] == "not-guaranteed"));
}

#[tokio::test]
async fn test_incident_four_slots_and_cancellation_before_queued_publication() {
    let permits = Arc::new(Semaphore::new(MAX_PENDING));
    let mut retained = vec![];
    for _ in 0..MAX_PENDING {
        retained.push(permits.clone().try_acquire_owned().unwrap());
    }
    assert!(permits.clone().try_acquire_owned().is_err());
    drop(retained);
    let root = tempfile::tempdir().unwrap();
    let lease = Arc::new(
        StateStore::open(&root.path().join("state.json"))
            .await
            .unwrap(),
    );
    let storage = Storage::open(root.path(), lease).unwrap();
    let (tx, rx) = mpsc::channel(8);
    let id = uuid::Uuid::new_v4().to_string();
    tx.try_send(storage::Work::Begin {
        capture: Box::new(evidence::Begin {
            schema_version: 1,
            id,
            identity: identity(root.path(), "run"),
            trigger: event(
                0,
                EventData::SpawnFailed {
                    failure: ProcessEvidence::Command,
                },
            ),
            observed_at: Utc::now(),
            observed_generation: None,
            state: None,
            prior: vec![],
            prior_omitted: 0,
            statistics: Statistics::default(),
        }),
        config: Arc::new(DevdConfig {
            version: "1".into(),
            dependency_timeout: std::time::Duration::from_secs(30),
            services: Default::default(),
        }),
        permit: Arc::new(permits.clone().try_acquire_owned().unwrap()),
        finished: Arc::new(AtomicBool::new(false)),
        deadline: std::time::Instant::now() + WINDOW,
    })
    .unwrap();
    drop(tx);
    tokio::task::spawn_blocking(move || storage.run(rx, Arc::new(AtomicBool::new(true))))
        .await
        .unwrap()
        .unwrap();
    assert!(records(root.path()).is_empty());
    assert_eq!(permits.available_permits(), MAX_PENDING);
}

#[test]
fn test_incident_configuration_summary_omits_values_and_bounds_graph() {
    let config: DevdConfig = serde_yaml::from_str("version: '1'\nservices:\n  api:\n    command: SECRET\n    env-file: SECRET\n    healthcheck: {type: script, command: SECRET}\n").unwrap();
    let text = serde_json::to_string(&evidence::configuration(&config)).unwrap();
    assert!(!text.contains("SECRET"));
    assert!(text.contains("healthcheck_configured"));
}

#[test]
fn test_incident_resource_inspection_marks_cancelled_and_expired_unknown() {
    let root = tempfile::tempdir().unwrap();
    let config: DevdConfig = serde_yaml::from_str("version: '1'\nservices:\n  api:\n    command: echo\n    requires: [{type: file, path: missing}]\n").unwrap();
    let capture = evidence::Begin {
        schema_version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        identity: identity(root.path(), "run"),
        trigger: event(
            0,
            EventData::SpawnFailed {
                failure: ProcessEvidence::Command,
            },
        ),
        observed_at: Utc::now(),
        observed_generation: Some(0),
        state: None,
        prior: vec![],
        prior_omitted: 0,
        statistics: Statistics::default(),
    };
    let finished = AtomicBool::new(false);
    let cancelled = AtomicBool::new(false);
    let report = evidence::resources(
        &capture,
        &config,
        root.path(),
        std::time::Instant::now(),
        &finished,
        &cancelled,
    );
    assert!(report.interrupted);
    assert_eq!(report.omitted, 1);
    assert!(report.observations.is_empty());
    finished.store(true, Ordering::Release);
    let report = evidence::resources(
        &capture,
        &config,
        root.path(),
        std::time::Instant::now() + WINDOW,
        &finished,
        &cancelled,
    );
    assert!(report.interrupted && report.observations.is_empty());
}

#[test]
fn test_incident_configuration_summary_selects_deterministic_bounded_names() {
    let mut services = serde_json::Map::new();
    for index in (0..260).rev() {
        services.insert(
            format!("service-{index:03}"),
            serde_json::json!({"command":"SECRET"}),
        );
    }
    let config: DevdConfig =
        serde_json::from_value(serde_json::json!({"version":"1","services":services})).unwrap();
    let report = serde_json::to_value(evidence::configuration(&config)).unwrap();
    assert_eq!(report["services"].as_array().unwrap().len(), 256);
    assert_eq!(report["omitted_services"], 4);
    assert_eq!(report["services"][0]["name"], "service-000");
    assert_eq!(report["services"][255]["name"], "service-255");
    assert!(!report.to_string().contains("SECRET"));
}

#[tokio::test]
async fn test_incident_saturation_reports_dropped_triggers_and_drains_pending() {
    let root = tempfile::tempdir().unwrap();
    let lease = Arc::new(
        StateStore::open(&root.path().join("state.json"))
            .await
            .unwrap(),
    );
    let storage = Storage::open(root.path(), lease).unwrap();
    let recorder = EventRecorder::new(root.path().join("state.json"), None);
    let (_, snapshots) = watch::channel(RuntimeSnapshot {
        supervisor_pid: 1,
        event_run_id: Some(recorder.run_id().into()),
        services: Default::default(),
    });
    let (_, configurations) = watch::channel(Arc::new(DevdConfig {
        version: "1".into(),
        dependency_timeout: std::time::Duration::from_secs(30),
        services: Default::default(),
    }));
    let (statistics, observed_statistics) = watch::channel(Statistics::default());
    let source = Source {
        identity: identity(root.path(), recorder.run_id()),
        events: recorder.subscribe(),
        snapshots,
        configurations,
        statistics,
    };
    for index in 0..10 {
        recorder.record(
            Some(&format!("service-{index}")),
            Some(index),
            None,
            EventData::SpawnFailed {
                failure: ProcessEvidence::Command,
            },
        );
    }
    drop(recorder);
    let stats = run(storage, source).await.unwrap();
    assert_eq!(*observed_statistics.borrow(), stats);
    assert_eq!(stats.admitted, 4);
    assert_eq!(stats.dropped_triggers, 6);
    assert!(stats.notice().unwrap().contains("6 triggers dropped"));
    let reports = records(root.path());
    assert_eq!(reports.len(), 4);
    assert!(reports
        .iter()
        .all(|r| r.last().unwrap()["data"]["statistics"]["dropped_triggers"] == 6));
}

#[tokio::test]
async fn test_incident_subscriber_lag_and_shutdown_are_visible() {
    let root = tempfile::tempdir().unwrap();
    let lease = Arc::new(
        StateStore::open(&root.path().join("state.json"))
            .await
            .unwrap(),
    );
    let storage = Storage::open(root.path(), lease).unwrap();
    let recorder = EventRecorder::new(root.path().join("state.json"), None);
    let (_, snapshots) = watch::channel(RuntimeSnapshot {
        supervisor_pid: 1,
        event_run_id: Some(recorder.run_id().into()),
        services: Default::default(),
    });
    let (_, configurations) = watch::channel(Arc::new(DevdConfig {
        version: "1".into(),
        dependency_timeout: std::time::Duration::from_secs(30),
        services: Default::default(),
    }));
    let (statistics, observed_statistics) = watch::channel(Statistics::default());
    let source = Source {
        identity: identity(root.path(), recorder.run_id()),
        events: recorder.subscribe(),
        snapshots,
        configurations,
        statistics,
    };
    for _ in 0..300 {
        recorder.record(Some("api"), Some(0), None, EventData::Starting);
    }
    recorder.record(
        Some("api"),
        Some(0),
        None,
        EventData::SpawnFailed {
            failure: ProcessEvidence::Command,
        },
    );
    recorder.record(
        None,
        None,
        None,
        EventData::SupervisorStopping {
            reason: crate::core::events::ShutdownReason::Requested,
        },
    );
    drop(recorder);
    let stats = run(storage, source).await.unwrap();
    assert_eq!(*observed_statistics.borrow(), stats);
    assert_eq!(stats.skipped_events, 46);
    let reports = records(root.path());
    assert_eq!(
        reports[0][0]["data"]["capture"]["statistics"]["skipped_events"],
        46
    );
    assert_eq!(reports[0].last().unwrap()["data"]["reason"], "stopping");
    assert!(
        reports[0][0]["data"]["capture"]["prior_omitted"]
            .as_u64()
            .unwrap()
            > 0
    );
}
