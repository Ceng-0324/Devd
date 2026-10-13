use super::*;
use crate::core::events::{EventData, EventRecorder};
use serde_json::{json, Value};

fn fixture() -> Value {
    let recorder = EventRecorder::new("/missing/state/services.json".into(), None);
    recorder.record(Some("api"), None, None, EventData::GenerationPending);
    recorder.record(Some("api"), Some(0), Some(0), EventData::Starting);
    recorder.record(
        Some("api"),
        Some(0),
        Some(1),
        EventData::Started { pid: Some(42) },
    );
    let window = recorder.history().snapshot();
    let now = Utc::now();
    json!({"schema_version":1,"identity":{
        "schema_version":1,"instance_id":"instance","run_id":recorder.run_id(),"supervisor_pid":42,
        "started_at":now,"config":"/missing/devd.yml","state_dir":"/missing/state","profile":null,"project_root":"/missing","git":null},
        "captured_from":now,"captured_to":now,"stable_during_capture":true,
        "event_watermark_after":3,"omitted_fields":[],"state":{},
        "events":{"schema_version":1,"source":"live","persistence":null,"context":window.context,
            "first_available":0,"cursor":{"run_id":recorder.run_id(),"next_sequence":3},"gaps":[],"omitted_gaps":0,"entries":window.entries.iter().map(|e| e.as_ref()).collect::<Vec<_>>()},
        "explanations":{"malicious":"FORGED-CONCLUSION"}})
}
fn parse(v: &Value) -> Result<Diagnosis> {
    validation::parse(&serde_json::to_vec(v).unwrap(), &Control::new())
}

#[test]
fn test_diagnose_legacy_live_recomputes_history_and_safely_renders() {
    let mut v = fixture(); // The old schema lacks source.
    let name = "api\u{1b}[2J\nforged-line\u{009b}\u{202e}";
    for e in v["events"]["entries"].as_array_mut().unwrap() {
        e["service"] = json!(name);
    }
    let d = parse(&v).unwrap();
    assert_eq!(d.source, "imported-report");
    assert_eq!(
        d.timeline.source,
        crate::core::events::query::EventSource::Stored
    );
    assert_eq!(
        d.explanations[name].conclusion,
        crate::core::diagnostics::ExplainConclusion::Running
    );
    let text = String::from_utf8(render(&d).unwrap()).unwrap();
    assert!(!text.contains('\u{1b}'));
    assert!(!text.contains('\u{009b}'));
    assert!(!text.contains('\u{202e}'));
    assert!(!text.contains("FORGED-CONCLUSION"));
    assert!(text.contains("\\u001b[2J\\nforged-line"));
    assert!(text.contains("\\u009b\\u202e"));
}

#[test]
fn test_diagnose_invalid_types_do_not_echo_terminal_controls() {
    let mut v = fixture();
    v["events"]["entries"][0]["data"]["type"] = json!("\u{1b}[2J");
    let error = parse(&v).err().expect("malformed event must fail");
    assert!(!format!("{error:#}").contains('\u{1b}'));
}

#[test]
fn test_diagnose_unknown_incident_is_unknown_and_never_uses_duplicate_events() {
    let mut v = fixture();
    v["schema_version"] = json!(3);
    v["incident"] = json!({"recorded":{"schema_version":1,"incident":{
        "id":uuid::Uuid::new_v4().to_string(),"artifact_schema_version":999,
        "beginning":null,"resources":null,"end":null,"complete":false
    }}});
    let d = parse(&v).unwrap();
    assert!(d.timeline.entries.is_empty());
    assert!(d.explanations.is_empty());
    assert_eq!(d.incident.unwrap().collection_status, "unsupported-schema");
    v["incident"]["recorded"]["schema_version"] = json!(999);
    assert!(parse(&v).is_err());
}

#[test]
fn test_diagnose_rejects_mixed_runs_future_causes_order_and_unknown_schemas() {
    for (pointer, replacement) in [
        ("/schema_version", json!(999)),
        ("/events/schema_version", json!(999)),
        ("/events/entries/1/schema_version", json!(999)),
        (
            "/events/entries/1/run_id",
            json!(uuid::Uuid::new_v4().to_string()),
        ),
        ("/events/entries/1/cause", json!(2)),
        ("/events/entries/1/generation", json!(2)),
        ("/events/entries/1/sequence", json!(0)),
        (
            "/events/cursor/run_id",
            json!(uuid::Uuid::new_v4().to_string()),
        ),
        ("/event_watermark_after", json!(1)),
    ] {
        let mut v = fixture();
        *v.pointer_mut(pointer).unwrap() = replacement;
        assert!(parse(&v).is_err(), "{pointer}");
    }
    assert!(validation::parse(b"{\"schema_version\":1", &Control::new()).is_err());
}

#[test]
fn test_diagnose_marks_discontinuities_even_if_input_claims_no_gaps() {
    let mut v = fixture();
    v["events"]["entries"].as_array_mut().unwrap().remove(1);
    let d = parse(&v).unwrap();
    assert!(d.timeline.gaps.iter().any(|g| matches!(
        g,
        crate::core::events::query::EventGap::StoredSequence { from: 1, to: 2, .. }
    )));
    assert!(!d.explanations["api"].complete);
    v["event_watermark_after"] = json!(4);
    assert!(parse(&v).unwrap().timeline.gaps.iter().any(|g| matches!(
        g,
        crate::core::events::query::EventGap::IncompleteRun { .. }
    )));
}

#[test]
fn test_diagnose_stored_state_is_stale_and_logs_stay_separate() {
    let mut v = fixture();
    let run = v["events"]["context"]["run_id"].clone();
    v["schema_version"] = json!(2);
    v["source"] = json!("stored");
    v["identity"] = Value::Null;
    v["stored"] = json!({"selection":{"requested_run":"latest"},"begin_retained":true,"end_retained":false,
        "historical_state":{"observed_run_id":run,"state":{}},
        "unassigned_logs":{"records":{"entries":[{"message":"PRIVATE-LOG"}]}}});
    let d = parse(&v).unwrap();
    assert_eq!(d.log_attachments[0].entries, 1);
    assert_eq!(
        d.log_attachments[0].attribution,
        "unknown-no-run-id-in-log-format"
    );
    assert!(!String::from_utf8(output::encode(&d).unwrap())
        .unwrap()
        .contains("PRIVATE-LOG"));
    assert!(!d.explanations["api"].complete);
    v["stored"]["historical_state"]["observed_run_id"] = json!(uuid::Uuid::new_v4().to_string());
    assert!(parse(&v).is_err());
}

#[test]
fn test_diagnose_input_limits_and_cancellation() {
    let v = fixture();
    let mut oversized = v.clone();
    oversized["events"]["entries"] = json!(vec![v["events"]["entries"][0].clone(); 1001]);
    assert!(parse(&oversized).is_err());
    oversized = v.clone();
    oversized["events"]["entries"][0]["service"] = json!("x".repeat(4097));
    assert!(parse(&oversized).is_err());
    let control = Control::new();
    control.cancel();
    assert!(validation::parse(&serde_json::to_vec(&v).unwrap(), &control).is_err());
    assert!(validation::parse(&serde_json::to_vec(&v).unwrap(), &Control::expired()).is_err());
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("report.json");
    std::fs::write(&file, serde_json::to_vec(&v).unwrap()).unwrap();
    assert!(output::read_regular(&file, &Control::new(), MAX_REPORT_BYTES).is_ok());
    let extra = root.path().join("hardlink");
    std::fs::hard_link(&file, &extra).unwrap();
    assert!(output::read_regular(&file, &Control::new(), MAX_REPORT_BYTES).is_err());
    std::fs::remove_file(extra).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(file).unwrap();
    f.set_len(MAX_REPORT_BYTES as u64 + 1).unwrap();
    assert!(output::read_regular(
        root.path().join("report.json").as_path(),
        &Control::new(),
        MAX_REPORT_BYTES
    )
    .is_err());
}

#[tokio::test]
async fn test_diagnose_output_cancels_under_backpressure() {
    let (mut sender, _receiver) = tokio::io::duplex(1);
    let result = write(&mut sender, b"large output", async {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    })
    .await;
    assert!(result.unwrap_err().to_string().contains("cancelled"));
}
