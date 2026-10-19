//! Validate structure and identities before deriving any historical explanation.
use super::{Control, Diagnosis, LogSummary, RunCoverage};
use crate::{
    cli::{export, incidents::query},
    core::{
        diagnostics,
        events::{
            query::{EventBatch, EventGap, EventSource},
            LifecycleEvent,
        },
    },
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn parse(bytes: &[u8], control: &Control) -> Result<Diagnosis> {
    control.check()?;
    if bytes.len() > super::MAX_REPORT_BYTES {
        bail!("report exceeds 16 MiB")
    }
    // serde_json's default depth limit remains enabled for untrusted input.
    let mut value: Value =
        serde_json::from_slice(bytes).context("invalid or truncated report JSON")?;
    let schema = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .context("missing report schema_version")?;
    if !matches!(schema, 1..=3) {
        bail!("unsupported report schema; upgrade devd")
    }
    let mut logs = Vec::new();
    for (pointer, location, attribution) in [
        (
            "/logs",
            "live-report",
            "captured-run-tail-not-incident-window",
        ),
        (
            "/stored/unassigned_logs/records/entries",
            "stored-run-attachment",
            "unknown-no-run-id-in-log-format",
        ),
        (
            "/incident/unassigned_stored_logs/records/entries",
            "incident-stored-attachment",
            "unknown-no-run-id-in-log-format",
        ),
        (
            "/incident/current_run_logs",
            "incident-live-attachment",
            "selected-run-tail-not-incident-window",
        ),
    ] {
        if let Some(v) = value.pointer(pointer).filter(|v| !v.is_null()) {
            let entries = v.as_array().context("invalid log attachment")?.len();
            if entries > export::LOG_LIMIT {
                bail!("log attachment exceeds 200 entries")
            }
            logs.push(LogSummary {
                location,
                entries,
                attribution,
            });
        }
    }
    let mut limitations = vec![
        "Historical imported data only; current health and processes are not inspected.",
        "Schema validation does not authenticate the report or prove causation.",
        "Paths may be sensitive; embedded paths, URLs and commands are never accessed or executed.",
        "Explanations are recomputed from retained events; imported explanations are ignored.",
        "Log contents are not displayed or used as incident evidence; attachment attribution is retained separately.",
    ];
    let mut stored_run_coverage = None;
    let mut incident = None;
    let mut incident_projection = None;
    let mut recorded_state: BTreeMap<String, export::SafeServiceState> = BTreeMap::new();
    if schema == 3 {
        if value
            .pointer("/incident/recorded/schema_version")
            .and_then(Value::as_u64)
            != Some(1)
        {
            bail!("unsupported incident query schema; upgrade devd")
        }
        let raw = value
            .pointer("/incident/recorded/incident")
            .context("missing recorded incident")?;
        let evidence = query::import(raw)?;
        incident_projection = Some(export::incident::describe(&evidence));
        incident = Some(evidence);
        // The embedded artifact is authoritative. Repeated top-level events,
        // states and derived explanations cannot replace the incident window.
        value["events"] = serde_json::to_value(&incident_projection.as_ref().unwrap().0)?;
        value["identity"] = Value::Null;
        value["source"] = json!("stored");
        value["state"] = json!({});
        limitations.push("Incident timeline comes only from its embedded recorded window; duplicate report fields are ignored.");
    } else {
        if value.get("incident").is_some_and(|v| !v.is_null()) {
            bail!("unexpected incident evidence for report schema")
        }
        if schema == 2 {
            if value.get("source") != Some(&json!("stored"))
                || value.get("identity").is_some_and(|v| !v.is_null())
                || value
                    .get("state")
                    .and_then(Value::as_object)
                    .is_none_or(|s| !s.is_empty())
            {
                bail!("stored report cannot claim a live identity")
            }
            let stored = value.get("stored").context("missing stored-run evidence")?;
            let selected_run = value
                .pointer("/events/context/run_id")
                .and_then(Value::as_str)
                .context("missing stored run identity")?
                .to_owned();
            let requested = stored
                .pointer("/selection/requested_run")
                .and_then(Value::as_str)
                .context("missing stored run selection")?;
            if requested != "latest" && requested != selected_run {
                bail!("stored run selection disagrees with event context")
            }
            if let Some(state) = stored
                .pointer("/historical_state/state")
                .filter(|v| !v.is_null())
            {
                if stored
                    .pointer("/historical_state/observed_run_id")
                    .and_then(Value::as_str)
                    != Some(selected_run.as_str())
                {
                    bail!("historical state belongs to a different or unknown run")
                }
                recorded_state = serde_json::from_value(state.clone())
                    .map_err(|_| anyhow::anyhow!("invalid historical state"))?;
            }
            let begin = stored.get("begin_retained").and_then(Value::as_bool) == Some(true);
            let end = stored.get("end_retained").and_then(Value::as_bool) == Some(true);
            stored_run_coverage = Some(RunCoverage {
                begin_reported_retained: begin,
                end_reported_retained: end,
                state_availability: stored
                    .pointer("/historical_state/availability")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned(),
            });
            if !begin || !end {
                let gaps = value
                    .pointer_mut("/events/gaps")
                    .and_then(Value::as_array_mut)
                    .context("invalid stored event gaps")?;
                gaps.push(serde_json::to_value(EventGap::IncompleteRun {
                    run_id: selected_run.to_owned(),
                })?);
            }
        } else if value.get("stored").is_some_and(|v| !v.is_null()) {
            bail!("unexpected stored-run evidence for live report")
        }
    }
    // Drop untrusted derived text and raw log content before typed decoding.
    value["explanations"] = json!({});
    value["logs"] = Value::Null;
    value["incident"] = Value::Null;
    value["stored"] = Value::Null;
    value["omitted_fields"] = json!([]);
    let mut report: export::Report = serde_json::from_value(value).map_err(|_| {
        anyhow::anyhow!("invalid report structure (fields, types or nested schema)")
    })?;
    control.check()?;
    if report.captured_to < report.captured_from {
        limitations.push("Capture wall clock moved backwards; timeline order follows event sequence, not timestamps.");
    }
    if schema == 1 {
        let identity = report
            .identity
            .as_ref()
            .context("missing historical live identity")?;
        if identity.schema_version != 1
            || serde_json::to_vec(identity)?.len() > 64 * 1024
            || report.events.context.as_ref().map(|c| c.run_id.as_str())
                != Some(identity.run_id.as_str())
        {
            bail!("invalid or inconsistent historical identity")
        }
        if report.source != EventSource::Live {
            bail!("invalid live report source")
        }
        recorded_state = std::mem::take(&mut report.state);
    }
    validate_state(&recorded_state)?;
    if schema != 3 {
        validate_batch(&mut report.events, report.event_watermark_after, control)?;
    }
    let (timeline, mut explanations) = if let Some(projection) = incident_projection {
        projection
    } else {
        let mut names: BTreeSet<String> = recorded_state.keys().cloned().collect();
        names.extend(
            report
                .events
                .entries
                .iter()
                .filter_map(|e| e.service.clone()),
        );
        if names.len() > export::SERVICE_LIMIT {
            bail!("report exceeds 256 services")
        }
        // Imported snapshots remain separately labelled stale facts. Explain
        // only the latest retained event generation, never a current process.
        let explanations = names
            .into_iter()
            .map(|name| {
                let mut explanation = diagnostics::explain(&name, None, &report.events);
                explanation
                    .details
                    .push("仅解释保留事件中的最新代次；采集时状态另列，不代表当前健康".into());
                (name, explanation)
            })
            .collect();
        (report.events, explanations)
    };
    for explanation in explanations.values_mut() {
        // Even a complete event window cannot establish health at import time.
        explanation.source = EventSource::Stored;
        explanation.persistence = None;
    }
    Ok(Diagnosis {
        schema_version: 1,
        source: "imported-report",
        input_schema_version: schema as u16,
        captured_from: report.captured_from,
        captured_to: report.captured_to,
        capture_reported_stable: report.stable_during_capture,
        current_health: "not-inspected-historical-evidence-only",
        recorded_state_attribution:
            "stale-capture-observation-not-current-health-or-event-explanation-input",
        stored_run_coverage,
        historical_identity: report.identity.or_else(|| {
            incident
                .as_ref()
                .and_then(|e| e.beginning.as_ref().map(|b| b.capture.identity.clone()))
        }),
        recorded_state,
        timeline,
        explanations,
        incident,
        log_attachments: logs,
        limitations,
    })
}

fn validate_state(state: &BTreeMap<String, export::SafeServiceState>) -> Result<()> {
    if state.len() > export::SERVICE_LIMIT || state.keys().any(|n| n.len() > 4096) {
        bail!("historical state exceeds service limits")
    }
    Ok(())
}

fn validate_batch(batch: &mut EventBatch, watermark: u64, control: &Control) -> Result<()> {
    if batch.entries.len() > export::EVENT_LIMIT
        || batch.gaps.len() > 129
        || serde_json::to_vec(&batch.gaps)?.len() > 64 * 1024
    {
        bail!("event or gap count exceeds report limits")
    }
    let context = batch
        .context
        .as_ref()
        .context("missing event run context")?;
    if uuid::Uuid::parse_str(&context.run_id).is_err()
        || serde_json::to_vec(context)?.len() > 16 * 1024
    {
        bail!("invalid event run context")
    }
    let run = context.run_id.clone();
    if batch.gaps.iter().any(|gap| matches!(gap,
        EventGap::Retention { from, to } | EventGap::StoredSequence { from, to, .. } if from > to
    )) {
        bail!("invalid event gap range")
    }
    let cursor = batch.cursor.as_ref().context("missing event cursor")?;
    if cursor.run_id != run || watermark < cursor.next_sequence {
        bail!("inconsistent event cursor or watermark")
    }
    let mut previous = None;
    let mut missing = Vec::new();
    for event in &mut batch.entries {
        control.check()?;
        validate_event(event, &run, previous)?;
        if let Some(sequence) = previous
            && event.sequence != sequence + 1
        {
            missing.push(EventGap::StoredSequence {
                run_id: run.clone(),
                from: sequence + 1,
                to: event.sequence,
            });
        }
        previous = Some(event.sequence);
        export::sanitize_event(event);
    }
    if batch
        .first_available
        .is_some_and(|n| batch.entries.first().is_some_and(|e| n > e.sequence))
        || previous.is_some_and(|n| n >= cursor.next_sequence)
    {
        bail!("inconsistent event boundaries")
    }
    if batch.entries.first().is_some_and(|e| e.sequence > 0) {
        missing.push(EventGap::Retention {
            from: 0,
            to: batch.entries[0].sequence,
        });
    }
    if batch.entries.is_empty()
        || previous.and_then(|n| n.checked_add(1)) != Some(cursor.next_sequence)
        || watermark != cursor.next_sequence
    {
        missing.push(EventGap::IncompleteRun { run_id: run });
    }
    for gap in missing {
        if !batch.gaps.contains(&gap) {
            if batch.gaps.len() < 129 {
                batch.gaps.push(gap);
            } else {
                batch.omitted_gaps = batch.omitted_gaps.saturating_add(1);
            }
        }
    }
    batch.source = EventSource::Stored;
    batch.persistence = None;
    Ok(())
}

fn validate_event(event: &LifecycleEvent, run: &str, previous: Option<u64>) -> Result<()> {
    if event.run_id != run
        || previous.is_some_and(|n| n >= event.sequence)
        || event.generation.is_some_and(|n| n > event.sequence)
        || event.cause.is_some_and(|n| n >= event.sequence)
        || event.service.as_ref().is_some_and(|s| s.len() > 4096)
        || serde_json::to_vec(event)?.len() > crate::core::events::MAX_EVENT_BYTES
    {
        bail!("invalid event identity, order, generation, cause or size")
    }
    Ok(())
}
