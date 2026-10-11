use super::*;
use crate::core::events::{EventData, EventRecorder};
use serde_json::{json, Value};

fn records() -> (String, Vec<Value>) {
    let id = uuid::Uuid::new_v4().to_string();
    let recorder = EventRecorder::new("/state/services.json".into(), None);
    recorder.record(Some("api"), None, None, EventData::GenerationPending);
    recorder.record(Some("api"), Some(0), Some(0), EventData::Starting);
    let window = recorder.history().snapshot();
    let now = chrono::Utc::now();
    let run = recorder.run_id();
    (
        id.clone(),
        vec![
            json!({"record":"begin","data":{"capture":{"schema_version":1,"id":id,"identity":{
            "schema_version":1,"instance_id":"instance","run_id":run,"supervisor_pid":123,"started_at":now,
            "config":"/devd.yml","state_dir":"/state","profile":null,"project_root":"/","git":null},
            "trigger":*window.entries[1],"prior":[*window.entries[0]],"observed_at":now,"observed_generation":0,"state":null,
            "prior_omitted":0,"statistics":{"admitted":1,"merged":0,"dropped_triggers":0,"skipped_events":0}},
            "configuration":{"source":"x","atomic":false,"trigger_time_equivalence":"unknown","services":[],"omitted_services":0},"retention_evicted_this_recorder":0}}),
            json!({"record":"resources","data":{"id":id,"source":"x","observed_from":now,"observed_to":now,"atomic":false,"trigger_time_equivalence":"unknown","observations":[],"omitted":0,"interrupted":false,"unavailable":0}}),
            json!({"record":"end","data":{"id":id,"reason":"deadline","observed_at":now,"following":[],"following_omitted":0,"merged_triggers":0,"skipped_events":0,"statistics":{"admitted":1,"merged":0,"dropped_triggers":0,"skipped_events":0}}}),
        ],
    )
}
fn bytes(records: &[Value]) -> Vec<u8> {
    records
        .iter()
        .flat_map(|r| format!("{r}\n").into_bytes())
        .collect()
}

#[test]
fn test_incident_decoder_preserves_prefix_schema_order_identity_and_privacy() {
    let (id, mut records) = records();
    assert!(model::parse(id.clone(), &bytes(&records)).complete);
    records[0]["data"]["capture"]["env"] = json!("SECRET");
    records[0]["data"]["configuration"]["source"] = json!("SECRET");
    let clean = model::parse(id.clone(), &bytes(&records));
    assert!(!serde_json::to_string(&clean).unwrap().contains("SECRET"));
    for count in 0..3 {
        let partial = model::parse(id.clone(), &bytes(&records[..count]));
        assert!(!partial.complete);
        assert_eq!(partial.beginning.is_some(), count > 0);
    }
    let mut unfinished = bytes(&records[..2]);
    unfinished.extend_from_slice(b"{\"record\":");
    let partial = model::parse(id.clone(), &unfinished);
    assert!(partial.resources.is_some());
    assert!(partial.diagnostics.iter().any(|s| s.contains("unfinished")));
    let mut extra = records.clone();
    extra.push(records[2].clone());
    assert!(!model::parse(id.clone(), &bytes(&extra)).complete);
    records[1]["data"]["id"] = json!(uuid::Uuid::new_v4().to_string());
    let wrong = model::parse(id.clone(), &bytes(&records));
    assert!(wrong.resources.is_none() && wrong.end.is_none());
    records[0]["data"]["capture"]["schema_version"] = json!(999);
    assert_eq!(
        model::parse(id, &bytes(&records)).collection_status,
        "unsupported-schema"
    );
}

#[test]
fn test_incident_decoder_rejects_cross_run_events_and_marks_all_coverage_gaps() {
    let (id, mut records) = records();
    records[0]["data"]["capture"]["prior"][0]["run_id"] = json!(uuid::Uuid::new_v4().to_string());
    assert!(model::parse(id, &bytes(&records)).beginning.is_none());
    let (id, mut records) = self::records();
    records[0]["data"]["capture"]["prior_omitted"] = json!(7);
    records[0]["data"]["retention_evicted_this_recorder"] = json!(3);
    records[1]["data"]["omitted"] = json!(1);
    records[2]["data"]["reason"] = json!("cancelled");
    records[2]["data"]["following_omitted"] = Value::Null;
    let report = model::parse(id, &bytes(&records));
    assert_eq!(report.collection_status, "interrupted");
    assert!(!report.complete);
    for gap in [
        "prior-context",
        "older-incidents",
        "resource-observation",
        "following-context",
        "ended-early",
    ] {
        assert!(report.diagnostics.iter().any(|s| s.contains(gap)), "{gap}");
    }
}

#[tokio::test]
async fn test_incident_readers_hold_writer_and_state_lease_and_cancel_gate_waits() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(
        StateStore::open(&root.path().join("services.json"))
            .await
            .unwrap(),
    );
    let storage = super::super::Storage::open(root.path(), state.clone()).unwrap();
    let reader = storage.reader();
    drop(storage);
    drop(state);
    assert!(Reader::offline(root.path()).is_err());
    let gate = reader.gate.clone();
    let guard = gate.lock().unwrap();
    let control = Control::new();
    control.cancel();
    assert!(reader
        .read(None, "instance".into(), None, &control)
        .is_err());
    drop(guard);
    drop(reader);
    assert!(Reader::offline(root.path()).unwrap().is_some());
}

#[tokio::test]
async fn test_incident_listing_bounds_and_missing_evidence_are_explicit() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(
        StateStore::open(&root.path().join("services.json"))
            .await
            .unwrap(),
    );
    let storage = super::super::Storage::open(root.path(), state).unwrap();
    let reader = storage.reader();
    for _ in 0..21 {
        let (id, records) = records();
        std::fs::write(
            root.path().join("incidents").join(format!("{id}.jsonl")),
            bytes(&records),
        )
        .unwrap();
    }
    let report = reader
        .read(None, "instance".into(), None, &Control::new())
        .unwrap();
    assert_eq!(report.entries.len(), 20);
    assert!(!report.complete);
    assert!(report.diagnostics.iter().any(|s| s.contains("limit-20")));
    assert!(report
        .entries
        .windows(2)
        .all(|p| p[0].observed_at >= p[1].observed_at));
    // Unmanaged names still consume enumeration work and must count toward
    // the directory scan limit (21 artifacts + lock + 235 unrelated entries).
    for n in 0..235 {
        std::fs::write(root.path().join("incidents").join(format!("extra-{n}")), []).unwrap();
    }
    assert!(reader
        .read(None, "instance".into(), None, &Control::new())
        .unwrap_err()
        .to_string()
        .contains("scan limit of 256"));
}

#[tokio::test]
async fn test_incident_reader_detects_replacement_and_releases_cancelled_reads() {
    let root = tempfile::tempdir().unwrap();
    let lease = Arc::new(
        StateStore::open(&root.path().join("services.json"))
            .await
            .unwrap(),
    );
    let storage = super::super::Storage::open(root.path(), lease).unwrap();
    let reader = storage.reader();
    let (id, records) = records();
    let path = root.path().join("incidents").join(format!("{id}.jsonl"));
    std::fs::write(&path, bytes(&records)).unwrap();
    let report = reader
        .read_one(&id, &Control::new(), || {
            std::fs::rename(&path, root.path().join("old-artifact")).unwrap();
            std::fs::write(&path, bytes(&records)).unwrap();
        })
        .unwrap();
    assert!(report.beginning.is_none());
    assert!(report
        .diagnostics
        .iter()
        .any(|s| s.contains("changed-during-read")));
    let control = Control::new();
    assert!(reader
        .read_one(&id, &control, || {
            control.cancel();
        })
        .is_err());
}

#[tokio::test(start_paused = true)]
async fn test_incident_output_timeout_and_cancellation_do_not_wait_for_reader() {
    let (mut writer, _reader) = tokio::io::duplex(1);
    assert!(
        write_report(&mut writer, "large output", std::future::pending())
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out")
    );
    let (mut writer, _reader) = tokio::io::duplex(1);
    assert!(write_report(&mut writer, "large output", async {})
        .await
        .unwrap_err()
        .to_string()
        .contains("cancelled"));
}

#[tokio::test]
async fn test_incident_cancelled_blocking_reader_retains_admission_until_return() {
    let root = tempfile::tempdir().unwrap();
    let lease = Arc::new(
        StateStore::open(&root.path().join("services.json"))
            .await
            .unwrap(),
    );
    let storage = super::super::Storage::open(root.path(), lease).unwrap();
    let reader = storage.reader();
    let gate = reader.gate.clone();
    let guard = gate.lock().unwrap();
    let permits = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = permits.clone().try_acquire_owned().unwrap();
    let control = Control::new();
    let worker_control = control.clone();
    let worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        reader.read(None, "instance".into(), None, &worker_control)
    });
    assert_eq!(permits.available_permits(), 0);
    control.cancel();
    drop(guard);
    assert!(worker.await.unwrap().is_err());
    assert_eq!(permits.available_permits(), 1);
}

#[test]
fn test_incident_wall_clock_regression_preserves_sequence_evidence() {
    let (id, mut records) = records();
    records[2]["data"]["observed_at"] = json!("2000-01-01T00:00:00Z");
    let evidence = model::parse(id, &bytes(&records));
    assert!(evidence.end.is_some());
    assert!(!evidence.complete);
    assert!(evidence
        .diagnostics
        .iter()
        .any(|s| s.contains("wall-clock")));
}
