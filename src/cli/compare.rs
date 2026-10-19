//! Offline comparison of two validated historical reports.
//!
//! Reports are evidence, never instructions. The comparison deliberately keeps
//! source identity, resource observations and log attachments separate so a
//! difference cannot be mistaken for a root-cause or content claim.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::export::{
    diagnose,
    output::{self, CancelOnDrop, Control, DEADLINE, MAX_REPORT_BYTES},
};

const MAX_CHANGES: usize = 2048;

#[derive(clap::Args)]
pub(super) struct Args {
    /// First transferred report (the left side of the comparison).
    left: PathBuf,
    /// Second transferred report (the right side of the comparison).
    right: PathBuf,
    /// Print one bounded JSON comparison report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct Comparison {
    schema_version: u16,
    source: &'static str,
    left: Side,
    right: Side,
    comparability: Vec<FieldComparison>,
    changes: Vec<Change>,
    limitations: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct Side {
    input_schema_version: Option<u16>,
    captured_from: Option<DateTime<Utc>>,
    captured_to: Option<DateTime<Utc>>,
    run_id: Option<String>,
    instance_id: Option<String>,
    profile: Option<String>,
    config_fingerprint: Option<String>,
    platform: &'static str,
    evidence_scope: &'static str,
    complete: bool,
}

#[derive(Debug, Serialize)]
struct FieldComparison {
    field: &'static str,
    status: &'static str,
    left: Option<String>,
    right: Option<String>,
    reason: &'static str,
}

#[derive(Debug, Serialize)]
struct Change {
    category: &'static str,
    key: String,
    status: &'static str,
    left: Option<String>,
    right: Option<String>,
    observed_left: Option<String>,
    observed_right: Option<String>,
    evidence: Vec<&'static str>,
}

pub(super) async fn run(args: Args) -> Result<()> {
    let mut signals = crate::platform::shutdown::Shutdown::new(false)?;
    let control = Control::new();
    let _cancel = CancelOnDrop(control.clone());
    let left = args.left;
    let right = args.right;
    let json = args.json;
    let work = async move {
        let (left, right) = tokio::join!(
            diagnose::load_json_with_control(&left, control.clone()),
            diagnose::load_json_with_control(&right, control.clone())
        );
        let report = compare(&left?, &right?, &control)?;
        control.check()?;
        let bytes = if json {
            output::encode(&report)?
        } else {
            render(&report)?
        };
        if bytes.len() > MAX_REPORT_BYTES {
            bail!("comparison output exceeds 16 MiB");
        }
        Ok::<_, anyhow::Error>(bytes)
    };
    let bytes = tokio::select! {
        biased;
        _ = signals.recv() => bail!("comparison cancelled"),
        result = tokio::time::timeout(DEADLINE, work) => result.context("comparison exceeded 30s")??,
    };
    let mut stdout = super::stdout::Stdout::new()?;
    write_output(&mut stdout, &bytes, signals.recv()).await
}

async fn write_output(
    stdout: &mut (impl tokio::io::AsyncWrite + Unpin),
    bytes: &[u8],
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    tokio::select! {
        biased;
        _ = cancelled => bail!("comparison output cancelled; output may be incomplete"),
        result = tokio::time::timeout(super::protocol::IO_TIMEOUT, async {
            stdout.write_all(bytes).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await
        }) => result.context("comparison output timed out; output may be incomplete")??,
    }
    Ok(())
}

fn compare(left: &Value, right: &Value, control: &Control) -> Result<Comparison> {
    control.check()?;
    let left_side = side(left)?;
    let right_side = side(right)?;
    let mut comparability = vec![];
    comparability.push(field(
        "schema",
        left_side.input_schema_version,
        right_side.input_schema_version,
        |a, b| a == b,
        "both reports use the same supported input schema",
        "schema versions differ; schema-specific fields are unknown rather than treated as changes",
        "one report does not expose a supported input schema",
    ));
    comparability.push(field(
        "profile",
        left_side.profile.clone(),
        right_side.profile.clone(),
        |a, b| a == b,
        "both reports identify the same profile",
        "profile declarations differ",
        "one report has no profile identity",
    ));
    comparability.push(field(
        "configuration-location",
        left_side.config_fingerprint.clone(),
        right_side.config_fingerprint.clone(),
        |a, b| a == b,
        "both reports refer to the same recorded configuration location",
        "recorded configuration locations differ; this is a source boundary, not a content comparison",
        "configuration identity was not retained",
    ));
    comparability.push(field(
        "instance",
        left_side.instance_id.clone(),
        right_side.instance_id.clone(),
        |a, b| a == b,
        "both reports identify the same devd instance",
        "reports come from different instance identities",
        "one report has no live instance identity",
    ));
    comparability.push(FieldComparison {
        field: "configuration-content",
        status: "unknown",
        left: None,
        right: None,
        reason: "export schemas retain configuration location or bounded incident summaries, not a complete configuration content identity",
    });
    comparability.push(FieldComparison {
        field: "platform",
        status: "unknown",
        left: Some("not-recorded".into()),
        right: Some("not-recorded".into()),
        reason: "the supported report schemas do not record an authoritative platform identity",
    });
    comparability.push(FieldComparison {
        field: "observation-scope",
        status: if left_side.evidence_scope == right_side.evidence_scope {
            "comparable"
        } else {
            "unknown"
        },
        left: Some(left_side.evidence_scope.into()),
        right: Some(right_side.evidence_scope.into()),
        reason: if left_side.evidence_scope == right_side.evidence_scope {
            "both sides expose the same kind of historical scope"
        } else {
            "different report scopes prevent an environmental equivalence claim"
        },
    });

    let mut changes = vec![];
    compare_states(left, right, &mut changes)?;
    compare_configurations(left, right, &mut changes)?;
    compare_incident_triggers(left, right, &mut changes)?;
    compare_gaps(left, right, &mut changes)?;
    compare_resources(left, right, &mut changes)?;
    compare_logs(left, right, &mut changes)?;
    if changes.len() > MAX_CHANGES {
        changes.truncate(MAX_CHANGES.saturating_sub(1));
        changes.push(Change {
            category: "report",
            key: "change-limit".into(),
            status: "unknown",
            left: None,
            right: None,
            observed_left: None,
            observed_right: None,
            evidence: vec!["comparison-output-truncated"],
        });
    }
    Ok(Comparison {
        schema_version: 1,
        source: "offline-report-comparison",
        left: left_side,
        right: right_side,
        comparability,
        changes,
        limitations: vec![
            "This is a comparison of transferred historical evidence; current health and the original project are not inspected.",
            "A changed link target or declaration is reported from retained metadata and observations; matching metadata never proves matching file contents.",
            "Similar symptoms, event kinds or resource observations do not establish a shared root cause, recurrence or recovery effect.",
            "Unknown means the required field was absent, truncated, schema-incomparable or outside the selected evidence scope.",
            "Log attachments remain separate metadata and are never treated as incident-window facts.",
        ],
    })
}

fn side(value: &Value) -> Result<Side> {
    let identity = value.get("historical_identity");
    let id = identity.filter(|v| !v.is_null());
    let input_schema_version = value
        .get("input_schema_version")
        .and_then(Value::as_u64)
        .map(|v| v as u16);
    let scope = if value.get("incident").is_some_and(|v| !v.is_null()) {
        "incident-window"
    } else if value.get("timeline").is_some() {
        "run-timeline"
    } else {
        "unknown"
    };
    let captured_from = value
        .get("captured_from")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok());
    let captured_to = value
        .get("captured_to")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok());
    let path = id.and_then(|v| v.get("config")).and_then(Value::as_str);
    Ok(Side {
        input_schema_version,
        captured_from,
        captured_to,
        run_id: id
            .and_then(|v| v.get("run_id"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        instance_id: id
            .and_then(|v| v.get("instance_id"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        profile: id
            .and_then(|v| v.get("profile"))
            .and_then(|v| v.as_str().map(str::to_owned)),
        config_fingerprint: path.map(fingerprint),
        platform: "unknown",
        evidence_scope: scope,
        complete: completeness(value),
    })
}

fn fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    format!("sha256:{digest:x}")
}

fn completeness(value: &Value) -> bool {
    let gaps = value
        .pointer("/timeline/gaps")
        .and_then(Value::as_array)
        .map_or(1, Vec::len);
    let omitted = value
        .pointer("/timeline/omitted_gaps")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    value
        .get("capture_reported_stable")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && gaps == 0
        && omitted == 0
        && value
            .pointer("/timeline/entries")
            .and_then(Value::as_array)
            .is_some_and(|entries| {
                entries
                    .iter()
                    .all(|event| event.get("truncated").and_then(Value::as_bool) == Some(false))
            })
}

fn field<T: ToString + Clone>(
    name: &'static str,
    left: Option<T>,
    right: Option<T>,
    same: impl Fn(&T, &T) -> bool,
    equal: &'static str,
    different: &'static str,
    unknown: &'static str,
) -> FieldComparison {
    let (status, reason) = match (&left, &right) {
        (Some(a), Some(b)) if same(a, b) => ("comparable", equal),
        (Some(_), Some(_)) => ("different", different),
        _ => ("unknown", unknown),
    };
    FieldComparison {
        field: name,
        status,
        left: left.map(|v| v.to_string()),
        right: right.map(|v| v.to_string()),
        reason,
    }
}

fn compare_states(left: &Value, right: &Value, changes: &mut Vec<Change>) -> Result<()> {
    let left_state = state_entries(left);
    let right_state = state_entries(right);
    if left_state.is_empty() && right_state.is_empty() {
        changes.push(unknown(
            "state",
            "recorded-state",
            "neither report scope contains a comparable service-state snapshot",
        ));
        compare_explanations(left, right, changes);
        return Ok(());
    }
    let names: BTreeSet<_> = left_state.keys().chain(right_state.keys()).collect();
    let left_observed = captured_to(left);
    let right_observed = captured_to(right);
    const LIFECYCLE_FIELDS: &[&str] = &[
        "kind",
        "status",
        "restart_count",
        "consecutive_failures",
        "last_exit_code",
        "last_exit_signal",
        "event_generation",
    ];
    for name in names {
        let left_service = left_state.get(name);
        let right_service = right_state.get(name);
        for field in LIFECYCLE_FIELDS {
            let l = left_service.and_then(|state| state.get(*field));
            let r = right_service.and_then(|state| state.get(*field));
            if l.is_none() && r.is_none() {
                continue;
            }
            if l.is_some() && r.is_some() && l == r {
                continue;
            }
            let status = if l.is_some() && r.is_some() {
                "different"
            } else {
                "unknown"
            };
            changes.push(Change {
                category: "state",
                key: format!("{name}.{field}"),
                status,
                left: l.map(value_text),
                right: r.map(value_text),
                observed_left: left_observed.clone(),
                observed_right: right_observed.clone(),
                evidence: vec!["captured-state-observation", "bounded-lifecycle-field"],
            });
        }
    }
    compare_explanations(left, right, changes);
    Ok(())
}

fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn compare_incident_triggers(left: &Value, right: &Value, changes: &mut Vec<Change>) -> Result<()> {
    let left_trigger = left.pointer("/incident/beginning/capture/trigger");
    let right_trigger = right.pointer("/incident/beginning/capture/trigger");
    match (left_trigger, right_trigger) {
        (None, None) => {}
        (Some(a), Some(b)) => {
            let left_data = a.get("data");
            let right_data = b.get("data");
            let left_time = left
                .pointer("/incident/beginning/capture/observed_at")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let right_time = right
                .pointer("/incident/beginning/capture/observed_at")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let complete = !a.get("truncated").and_then(Value::as_bool).unwrap_or(true)
                && !b.get("truncated").and_then(Value::as_bool).unwrap_or(true);
            let (status, changed) = match (left_data, right_data) {
                (Some(left), Some(right)) if complete && left == right => ("comparable", false),
                (Some(_), Some(_)) if complete => ("different", true),
                _ => ("unknown", true),
            };
            if changed {
                changes.push(Change {
                    category: "incident-trigger",
                    key: "event-data".into(),
                    status,
                    left: left_data.map(|value| fingerprint(&value.to_string())),
                    right: right_data.map(|value| fingerprint(&value.to_string())),
                    observed_left: left_time,
                    observed_right: right_time,
                    evidence: vec!["recorded-incident-trigger", "not-shared-cause"],
                });
            }
        }
        _ => changes.push(unknown(
            "incident-trigger",
            "event-data",
            "an incident trigger is absent from one report scope",
        )),
    }
    Ok(())
}

fn state_entries(report: &Value) -> BTreeMap<String, Value> {
    let retained = report.get("recorded_state").and_then(Value::as_object);
    if let Some(retained) = retained.filter(|state| !state.is_empty()) {
        return retained
            .iter()
            .map(|(name, state)| (name.clone(), state.clone()))
            .collect();
    }
    let incident_state = report.pointer("/incident/beginning/capture/state");
    let service = report
        .pointer("/incident/beginning/capture/trigger/service")
        .and_then(Value::as_str);
    match (service, incident_state.filter(|state| !state.is_null())) {
        (Some(service), Some(state)) => BTreeMap::from([(service.to_owned(), state.clone())]),
        _ => BTreeMap::new(),
    }
}

fn compare_configurations(left: &Value, right: &Value, changes: &mut Vec<Change>) -> Result<()> {
    let left = left.pointer("/incident/beginning/configuration");
    let right = right.pointer("/incident/beginning/configuration");
    let (Some(left), Some(right)) = (left, right) else {
        changes.push(unknown(
            "configuration",
            "declared-runtime-summary",
            "configuration summaries are not retained by both report scopes",
        ));
        return Ok(());
    };
    for (configuration, side) in [(left, "left"), (right, "right")] {
        let size = serde_json::to_vec(configuration)?.len();
        if size > 128 * 1024
            || configuration
                .get("services")
                .and_then(Value::as_array)
                .is_none()
        {
            bail!("{side} incident configuration summary is invalid or exceeds its schema limit");
        }
    }
    let complete = |value: &Value| value.get("omitted_services").and_then(Value::as_u64) == Some(0);
    let a: BTreeMap<_, _> = left["services"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|service| Some((service.get("name")?.as_str()?.to_owned(), service)))
        .collect();
    let b: BTreeMap<_, _> = right["services"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|service| Some((service.get("name")?.as_str()?.to_owned(), service)))
        .collect();
    let names: BTreeSet<_> = a.keys().chain(b.keys()).collect();
    for name in names {
        let (Some(a), Some(b)) = (a.get(name), b.get(name)) else {
            changes.push(Change {
                category: "declared-service",
                key: name.clone(),
                status: if complete(left) && complete(right) {
                    "different"
                } else {
                    "unknown"
                },
                left: a.get(name).map(|v| fingerprint(&v.to_string())),
                right: b.get(name).map(|v| fingerprint(&v.to_string())),
                observed_left: left
                    .get("observed_at")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                observed_right: right
                    .get("observed_at")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                evidence: vec!["retained-declared-configuration-summary"],
            });
            continue;
        };
        if a == b {
            continue;
        }
        changes.push(Change {
            category: "declared-service",
            key: name.clone(),
            status: if complete(left) && complete(right) {
                "different"
            } else {
                "unknown"
            },
            left: Some(fingerprint(&a.to_string())),
            right: Some(fingerprint(&b.to_string())),
            observed_left: left
                .get("observed_at")
                .and_then(Value::as_str)
                .map(str::to_owned),
            observed_right: right
                .get("observed_at")
                .and_then(Value::as_str)
                .map(str::to_owned),
            evidence: vec!["retained-declared-configuration-summary"],
        });
    }
    if !complete(left) || !complete(right) {
        changes.push(unknown(
            "configuration",
            "omitted-services",
            "at least one retained configuration summary is partial",
        ));
    }
    Ok(())
}

fn compare_explanations(left: &Value, right: &Value, changes: &mut Vec<Change>) {
    let a = left.get("explanations").and_then(Value::as_object);
    let b = right.get("explanations").and_then(Value::as_object);
    let (Some(a), Some(b)) = (a, b) else {
        return;
    };
    let names: BTreeSet<_> = a.keys().chain(b.keys()).collect();
    for name in names {
        let l = a
            .get(name)
            .and_then(|v| v.get("conclusion"))
            .and_then(Value::as_str);
        let r = b
            .get(name)
            .and_then(|v| v.get("conclusion"))
            .and_then(Value::as_str);
        if l == r && l.is_some() {
            continue;
        }
        changes.push(Change {
            category: "historical-explanation",
            key: name.clone(),
            status: if l.is_some() && r.is_some() {
                "different"
            } else {
                "unknown"
            },
            left: l.map(str::to_owned),
            right: r.map(str::to_owned),
            observed_left: captured_to(left),
            observed_right: captured_to(right),
            evidence: vec!["recomputed-from-retained-events", "not-causation"],
        });
    }
}

fn compare_gaps(left: &Value, right: &Value, changes: &mut Vec<Change>) -> Result<()> {
    let l = gap_summaries(left)?;
    let r = gap_summaries(right)?;
    let keys: BTreeSet<_> = l.keys().chain(r.keys()).collect();
    for key in keys {
        let a = l.get(key);
        let b = r.get(key);
        let count_a = a.map_or(0, |gap| gap.count);
        let count_b = b.map_or(0, |gap| gap.count);
        if count_a != count_b {
            changes.push(Change {
                category: "evidence-gap",
                key: key.clone(),
                status: if a.is_some() && b.is_some() {
                    "different"
                } else {
                    "unknown"
                },
                left: Some(count_a.to_string()),
                right: Some(count_b.to_string()),
                observed_left: captured_to(left),
                observed_right: captured_to(right),
                evidence: vec!["retained-event-gap-metadata"],
            });
        }
        if let (Some(a), Some(b)) = (a, b)
            && a.payloads != b.payloads
        {
            changes.push(Change {
                category: "evidence-gap",
                key: format!("{key}/payload"),
                status: "different",
                left: Some(fingerprint(&serde_json::to_string(&a.payloads)?)),
                right: Some(fingerprint(&serde_json::to_string(&b.payloads)?)),
                observed_left: captured_to(left),
                observed_right: captured_to(right),
                evidence: vec!["retained-event-gap-metadata"],
            });
        }
    }
    let omitted = |report: &Value| {
        report
            .pointer("/timeline/omitted_gaps")
            .and_then(Value::as_u64)
    };
    let (a, b) = (omitted(left), omitted(right));
    if a != b {
        changes.push(Change {
            category: "evidence-gap",
            key: "omitted-gaps".into(),
            status: if a.is_some() && b.is_some() {
                "different"
            } else {
                "unknown"
            },
            left: a.map(|n| n.to_string()),
            right: b.map(|n| n.to_string()),
            observed_left: captured_to(left),
            observed_right: captured_to(right),
            evidence: vec!["retained-event-gap-metadata"],
        });
    }
    Ok(())
}

fn captured_to(value: &Value) -> Option<String> {
    value
        .get("captured_to")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[derive(Default)]
struct GapSummary {
    count: usize,
    payloads: BTreeMap<String, usize>,
}

fn gap_summaries(value: &Value) -> Result<BTreeMap<String, GapSummary>> {
    use crate::core::events::query::EventGap;
    let mut result: BTreeMap<String, GapSummary> = BTreeMap::new();
    let Some(gaps) = value.pointer("/timeline/gaps").and_then(Value::as_array) else {
        return Ok(result);
    };
    for gap in gaps {
        // Use the published enum to normalize payloads and bound reason keys.
        // Compare a multiset rather than record order; identifiers stay hashed.
        let gap: EventGap = serde_json::from_value(gap.clone())?;
        let mut payload = serde_json::to_value(gap)?;
        let reason = payload.as_object_mut().unwrap().remove("reason").unwrap();
        let summary = result
            .entry(reason.as_str().unwrap().to_owned())
            .or_default();
        summary.count += 1;
        *summary
            .payloads
            .entry(fingerprint(&payload.to_string()))
            .or_default() += 1;
    }
    Ok(result)
}

fn compare_resources(left: &Value, right: &Value, changes: &mut Vec<Change>) -> Result<()> {
    let l = resources(left);
    let r = resources(right);
    let (Some(l), Some(r)) = (l, r) else {
        changes.push(unknown(
            "resource",
            "declared-relationships",
            "one side has no retained resource relationship evidence",
        ));
        return Ok(());
    };
    let l_graph = l.get("relationships");
    let r_graph = r.get("relationships");
    match (l_graph, r_graph) {
        (Some(a), Some(b)) if !a.is_null() && !b.is_null() => {
            compare_graph_edges(a, b, changes);
            compare_graph_coverage(a, b, changes);
        }
        (Some(a), Some(b)) if a.is_null() && b.is_null() => changes.push(unknown(
            "resource",
            "declared-relationships",
            "both reports retain resources but neither has a usable relationship graph",
        )),
        _ => changes.push(unknown(
            "resource",
            "declared-relationships",
            "relationship graph is missing or unavailable on one side",
        )),
    }
    let l_rows = resource_rows(l);
    let r_rows = resource_rows(r);
    let keys: BTreeSet<_> = l_rows.keys().chain(r_rows.keys()).collect();
    for key in keys {
        let a = l_rows.get(key);
        let b = r_rows.get(key);
        match (a, b) {
            (Some(a), Some(b)) if a.0 == b.0 && a.2 && b.2 => {}
            (Some(a), Some(b)) => changes.push(Change {
                category: "resource-observation",
                key: key.clone(),
                status: if a.2 && b.2 { "different" } else { "unknown" },
                left: Some(a.0.clone()),
                right: Some(b.0.clone()),
                observed_left: Some(a.1.clone()),
                observed_right: Some(b.1.clone()),
                evidence: vec![
                    "retained-resource-metadata",
                    "matching metadata-does-not-prove-content",
                ],
            }),
            _ => changes.push(unknown(
                "resource-observation",
                key,
                "resource selector is present on only one side; the other side may be unknown",
            )),
        }
    }
    Ok(())
}

fn resources(value: &Value) -> Option<&Value> {
    value
        .pointer("/incident/resources")
        .filter(|v| v.is_object())
}

fn resource_rows(resources: &Value) -> BTreeMap<String, (String, String, bool)> {
    let mut rows = BTreeMap::new();
    let collection_complete = resources.get("omitted").and_then(Value::as_u64) == Some(0)
        && resources.get("unavailable").and_then(Value::as_u64) == Some(0)
        && resources.get("interrupted").and_then(Value::as_bool) == Some(false);
    let Some(items) = resources.get("observations").and_then(Value::as_array) else {
        return rows;
    };
    for item in items {
        // Resource fields are flattened in the incident evidence schema.
        let target = item;
        let selector = target.get("selector").cloned().unwrap_or(Value::Null);
        let key = selector.to_string();
        let requested = target
            .get("requested_path")
            .and_then(Value::as_str)
            .map(fingerprint)
            .unwrap_or_else(|| "unknown".into());
        let inspection = target.get("inspection");
        let observed = inspection
            .and_then(|v| v.get("resolved"))
            .and_then(Value::as_str)
            .map(fingerprint)
            .unwrap_or_else(|| "unknown".into());
        let chain: Vec<_> = inspection
            .and_then(|v| v.get("nodes"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|node| {
                json!({
                    "path": node.get("path").and_then(Value::as_str).map(fingerprint),
                    "kind": node.get("kind"),
                    "target": node.get("target").and_then(Value::as_str).map(fingerprint),
                })
            })
            .collect();
        let status = format!(
            "requested={requested}; observed={observed}; chain={}; failure={}; complete={}",
            fingerprint(&Value::Array(chain).to_string()),
            inspection
                .and_then(|v| v.get("failure"))
                .map_or_else(|| "unknown".into(), Value::to_string),
            inspection
                .and_then(|v| v.get("complete"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        );
        let observed_at = resources
            .get("observed_from")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let complete = collection_complete
            && inspection
                .and_then(|v| v.get("complete"))
                .and_then(Value::as_bool)
                == Some(true);
        rows.insert(key, (status, observed_at, complete));
    }
    rows
}

fn compare_graph_edges(left: &Value, right: &Value, changes: &mut Vec<Change>) {
    let edges = |value: &Value| -> BTreeSet<String> {
        value
            .get("dependencies")
            .and_then(Value::as_array)
            .map(|xs| xs.iter().map(Value::to_string).collect())
            .unwrap_or_default()
    };
    let l = edges(left);
    let r = edges(right);
    let complete = |graph: &Value| {
        ["omitted_services", "omitted_dependencies"]
            .iter()
            .all(|field| graph.get(*field).and_then(Value::as_u64) == Some(0))
    };
    for edge in l.symmetric_difference(&r) {
        changes.push(Change {
            category: "declared-dependency",
            key: edge.clone(),
            status: if complete(left) && complete(right) {
                "different"
            } else {
                "unknown"
            },
            left: Some(l.contains(edge).to_string()),
            right: Some(r.contains(edge).to_string()),
            observed_left: left
                .get("observed_from")
                .and_then(Value::as_str)
                .map(str::to_owned),
            observed_right: right
                .get("observed_from")
                .and_then(Value::as_str)
                .map(str::to_owned),
            evidence: vec!["recorded-dependency-edge"],
        });
    }
}

fn compare_graph_coverage(left: &Value, right: &Value, changes: &mut Vec<Change>) {
    for field in [
        "omitted_services",
        "omitted_dependencies",
        "unavailable_impacts",
        "omitted_impacts",
        "inspections_performed",
    ] {
        let a = left.get(field).and_then(Value::as_u64);
        let b = right.get(field).and_then(Value::as_u64);
        if a == b && a.is_some() {
            continue;
        }
        changes.push(Change {
            category: "relationship-coverage",
            key: field.into(),
            status: if a.is_some() && b.is_some() {
                "different"
            } else {
                "unknown"
            },
            left: a.map(|v| v.to_string()),
            right: b.map(|v| v.to_string()),
            observed_left: left
                .get("observed_from")
                .and_then(Value::as_str)
                .map(str::to_owned),
            observed_right: right
                .get("observed_from")
                .and_then(Value::as_str)
                .map(str::to_owned),
            evidence: vec!["recorded-relationship-coverage"],
        });
    }
    for field in ["interrupted", "trigger_time_equivalence"] {
        let a = left.get(field);
        let b = right.get(field);
        if a == b && a.is_some() {
            continue;
        }
        changes.push(Change {
            category: "relationship-coverage",
            key: field.into(),
            status: if a.is_some() && b.is_some() {
                "different"
            } else {
                "unknown"
            },
            left: a.map(Value::to_string),
            right: b.map(Value::to_string),
            observed_left: left
                .get("observed_from")
                .and_then(Value::as_str)
                .map(str::to_owned),
            observed_right: right
                .get("observed_from")
                .and_then(Value::as_str)
                .map(str::to_owned),
            evidence: vec!["recorded-relationship-coverage"],
        });
    }
}

fn compare_logs(left: &Value, right: &Value, changes: &mut Vec<Change>) -> Result<()> {
    let a = left
        .get("log_attachments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let b = right
        .get("log_attachments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if a.is_empty() && b.is_empty() {
        return Ok(());
    }
    let summaries = |items: &[Value]| -> BTreeMap<String, (usize, String)> {
        items
            .iter()
            .filter_map(|v| {
                let location = v.get("location")?.as_str()?.to_owned();
                let count = v.get("entries")?.as_u64()? as usize;
                let attribution = v.get("attribution")?.as_str()?.to_owned();
                Some((location, (count, attribution)))
            })
            .collect()
    };
    let a = summaries(&a);
    let b = summaries(&b);
    let locations: BTreeSet<_> = a.keys().chain(b.keys()).collect();
    for location in locations {
        if a.get(location) == b.get(location) {
            continue;
        }
        changes.push(Change {
            category: "log-attachment",
            key: location.clone(),
            status: if a.contains_key(location) && b.contains_key(location) {
                "different"
            } else {
                "unknown"
            },
            left: a
                .get(location)
                .map(|(count, attribution)| format!("{count} entries; {attribution}")),
            right: b
                .get(location)
                .map(|(count, attribution)| format!("{count} entries; {attribution}")),
            observed_left: None,
            observed_right: None,
            evidence: vec!["log-attachment-metadata-only", "not-incident-window"],
        });
    }
    Ok(())
}

fn unknown(category: &'static str, key: impl Into<String>, reason: &'static str) -> Change {
    Change {
        category,
        key: key.into(),
        status: "unknown",
        left: None,
        right: None,
        observed_left: None,
        observed_right: None,
        evidence: vec![reason],
    }
}

fn render(comparison: &Comparison) -> Result<Vec<u8>> {
    let mut safe = output::BoundedBytes(Vec::new());
    use std::io::Write;
    writeln!(safe, "Offline comparison — historical evidence only")?;
    writeln!(
        safe,
        "Left: schema={:?}, scope={:?}, complete={}",
        comparison.left.input_schema_version,
        comparison.left.evidence_scope,
        comparison.left.complete
    )?;
    writeln!(
        safe,
        "Right: schema={:?}, scope={:?}, complete={}",
        comparison.right.input_schema_version,
        comparison.right.evidence_scope,
        comparison.right.complete
    )?;
    writeln!(safe, "Comparability:")?;
    for field in &comparison.comparability {
        writeln!(
            safe,
            "  {}: {} ({})",
            field.field, field.status, field.reason
        )?;
    }
    writeln!(safe, "Changes:")?;
    if comparison.changes.is_empty() {
        writeln!(safe, "  none observed in comparable evidence")?;
    }
    for change in &comparison.changes {
        writeln!(
            safe,
            "  [{}] {} / {}: left={:?}, right={:?}",
            change.status, change.category, change.key, change.left, change.right
        )?;
    }
    writeln!(safe, "Limitations:")?;
    for limitation in &comparison.limitations {
        writeln!(safe, "  {limitation}")?;
    }
    let text = String::from_utf8(safe.0)?;
    let mut safe = output::BoundedBytes(Vec::new());
    for c in text.chars() {
        if matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            && !matches!(c, '\n' | '\r' | '\t')
        {
            write!(safe, "\\u{:04x}", c as u32)?;
        } else {
            write!(safe, "{c}")?;
        }
    }
    Ok(safe.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn diagnosis() -> Value {
        json!({"schema_version":1,"source":"imported-report","input_schema_version":1,
            "captured_from":"2026-01-01T00:00:00Z","captured_to":"2026-01-01T00:00:01Z",
            "capture_reported_stable":true,"current_health":"not-inspected-historical-evidence-only",
            "recorded_state_attribution":"stale","stored_run_coverage":null,
            "historical_identity":{"schema_version":1,"instance_id":"i","run_id":"r","supervisor_pid":1,"started_at":"2026-01-01T00:00:00Z","config":"/secret/devd.yml","state_dir":"/secret/.devd","profile":"dev","project_root":"/secret","git":null},
            "recorded_state":{"api":{"status":"running"}},
            "timeline":{"schema_version":1,"source":"stored","persistence":null,"context":{"run_id":"r","state_path":"/secret/.devd/services.json","profile":"dev"},"first_available":0,"cursor":{"run_id":"r","next_sequence":1},"gaps":[],"omitted_gaps":0,"entries":[]},"explanations":{"api":{"conclusion":"running"}},"incident":null,"log_attachments":[],"limitations":[]})
    }

    #[test]
    fn test_compare_distinguishes_equal_different_unknown_and_hides_paths() {
        let mut right = diagnosis();
        right["recorded_state"]["api"]["status"] = json!("failed");
        right["historical_identity"]["config"] = json!("/other/devd.yml");
        let result = compare(&diagnosis(), &right, &Control::new()).unwrap();
        assert!(result
            .comparability
            .iter()
            .any(|f| f.field == "configuration-location" && f.status == "different"));
        assert!(result
            .changes
            .iter()
            .any(|c| c.category == "state" && c.status == "different"));
        assert!(serde_json::to_string(&result).unwrap().contains("sha256:"));
        assert!(!serde_json::to_string(&result).unwrap().contains("/secret"));
    }

    #[test]
    fn test_compare_reports_bounded_lifecycle_field_changes() {
        let mut left = diagnosis();
        left["recorded_state"]["api"] = json!({
            "status": "running",
            "restart_count": 0,
            "event_generation": 1,
            "consecutive_failures": 0
        });
        let mut right = left.clone();
        right["recorded_state"]["api"]["restart_count"] = json!(2);
        right["recorded_state"]["api"]["event_generation"] = json!(3);
        let result = compare(&left, &right, &Control::new()).unwrap();
        assert!(result.changes.iter().any(|change| {
            change.category == "state"
                && change.key == "api.restart_count"
                && change.status == "different"
                && change.left.as_deref() == Some("0")
                && change.right.as_deref() == Some("2")
        }));
        assert!(result.changes.iter().any(|change| {
            change.category == "state"
                && change.key == "api.event_generation"
                && change.status == "different"
        }));
        assert!(!result
            .changes
            .iter()
            .any(|change| change.key == "api.consecutive_failures"));
    }

    #[test]
    fn test_compare_missing_lifecycle_field_is_unknown() {
        let mut left = diagnosis();
        left["recorded_state"]["api"] = json!({"status":"running"});
        let mut right = left.clone();
        right["recorded_state"]["api"]["restart_count"] = json!(1);
        let result = compare(&left, &right, &Control::new()).unwrap();
        assert!(result.changes.iter().any(|change| {
            change.category == "state"
                && change.key == "api.restart_count"
                && change.status == "unknown"
        }));
    }

    #[test]
    fn test_compare_missing_resources_and_gaps_are_unknown_not_changes() {
        let mut right = diagnosis();
        right["timeline"]["gaps"] = json!([{"reason":"incomplete-run","run_id":"r"}]);
        right["historical_identity"] = Value::Null;
        let result = compare(&diagnosis(), &right, &Control::new()).unwrap();
        assert!(result
            .changes
            .iter()
            .any(|c| c.category == "resource" && c.status == "unknown"));
        assert!(result
            .changes
            .iter()
            .any(|c| c.category == "evidence-gap" && c.status == "unknown"));
        assert!(result
            .comparability
            .iter()
            .any(|f| f.field == "instance" && f.status == "unknown"));
    }

    #[test]
    fn test_compare_gap_reasons_payloads_counts_and_omissions() {
        let mut left = diagnosis();
        left["timeline"]["gaps"] = json!([{"reason":"recovered-tail","bytes":12}]);
        for (gaps, omitted, key) in [
            (
                json!([{"reason":"unfinished-record","bytes":12}]),
                0,
                "unfinished-record",
            ),
            (
                json!([{"reason":"recovered-tail","bytes":24}]),
                0,
                "recovered-tail/payload",
            ),
            (
                json!([{"reason":"recovered-tail","bytes":12}, {"reason":"recovered-tail","bytes":12}]),
                0,
                "recovered-tail",
            ),
            (left["timeline"]["gaps"].clone(), 4, "omitted-gaps"),
        ] {
            let mut right = left.clone();
            right["timeline"]["gaps"] = gaps;
            right["timeline"]["omitted_gaps"] = json!(omitted);
            let result = compare(&left, &right, &Control::new()).unwrap();
            assert!(
                result
                    .changes
                    .iter()
                    .any(|c| c.category == "evidence-gap" && c.key == key),
                "missing {key}: {:?}",
                result.changes
            );
        }
    }

    #[test]
    fn test_compare_gap_payloads_are_order_independent_and_private() {
        let gaps = json!([
            {"reason":"subscriber-lag","run_id":"private-run","skipped":3},
            {"reason":"subscriber-lag","run_id":"private-run","skipped":5}
        ]);
        let mut left = diagnosis();
        left["timeline"]["gaps"] = gaps;
        let mut right = left.clone();
        right["timeline"]["gaps"].as_array_mut().unwrap().reverse();
        for (a, b) in [(&left, &right), (&diagnosis(), &diagnosis())] {
            assert!(!compare(a, b, &Control::new())
                .unwrap()
                .changes
                .iter()
                .any(|c| c.category == "evidence-gap"));
        }
        right["timeline"]["gaps"][0]["skipped"] = json!(6);
        let result = compare(&left, &right, &Control::new()).unwrap();
        let change = result
            .changes
            .iter()
            .find(|c| c.key == "subscriber-lag/payload")
            .unwrap();
        assert_eq!(change.status, "different");
        assert!(change.left.as_ref().unwrap().starts_with("sha256:"));
        assert_ne!(change.left, change.right);
        assert!(!serde_json::to_string(&result)
            .unwrap()
            .contains("private-run"));
    }

    #[test]
    fn test_compare_completeness_requires_untruncated_retained_events() {
        let mut left = diagnosis();
        left["timeline"]["entries"] = json!([{"truncated":false}]);
        let mut right = left.clone();
        right["timeline"]["entries"][0]["truncated"] = json!(true);
        let result = compare(&left, &right, &Control::new()).unwrap();
        assert!(result.left.complete);
        assert!(!result.right.complete);
    }

    #[test]
    fn test_compare_explanations_when_recorded_state_is_absent() {
        let mut left = diagnosis();
        left["recorded_state"] = json!({});
        let mut right = left.clone();
        right["explanations"]["api"]["conclusion"] = json!("startup-failed");

        let result = compare(&left, &right, &Control::new()).unwrap();
        assert!(result
            .changes
            .iter()
            .any(|change| change.category == "state" && change.key == "recorded-state"));
        assert!(result.changes.iter().any(|change| {
            change.category == "historical-explanation"
                && change.key == "api"
                && change.left.as_deref() == Some("running")
                && change.right.as_deref() == Some("startup-failed")
        }));
    }

    #[test]
    fn test_compare_link_target_change_keeps_observation_times_and_no_causation_claim() {
        let resources = |target: &str| {
            json!({
                "observed_from":"2026-01-01T00:00:00Z",
                "observed_to":"2026-01-01T00:00:01Z",
                "omitted":0,"interrupted":false,"unavailable":0,
                "observations":[{"selector":{"service":"api","field":{"requires":{"index":0}}},"requested_path":"/private/link","inspection":{"resolved":null,"failure":"dangling-symlink","nodes":[{"path":"/private/link","kind":"symlink","target":target}],"complete":true}}],
                "relationships":{"observed_from":"2026-01-01T00:00:00Z","observed_to":"2026-01-01T00:00:01Z","dependencies":[],"services":["api"]}
            })
        };
        let mut left = diagnosis();
        left["incident"] = json!({"resources":resources("/private/target-a")});
        let mut right = diagnosis();
        right["incident"] = json!({"resources":resources("/private/target-b")});
        let result = compare(&left, &right, &Control::new()).unwrap();
        let change = result
            .changes
            .iter()
            .find(|c| c.category == "resource-observation")
            .unwrap();
        assert_eq!(change.status, "different");
        assert_eq!(
            change.observed_left.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert!(result
            .limitations
            .iter()
            .any(|text| text.contains("root cause")));
        let encoded = serde_json::to_string(&result).unwrap();
        assert!(!encoded.contains("/private/target"));
    }

    #[test]
    fn test_compare_incomplete_resource_observation_is_unknown() {
        let resources = |target: &str| {
            json!({
                "observed_from":"2026-01-01T00:00:00Z",
                "observed_to":"2026-01-01T00:00:01Z",
                "observations":[{"selector":{"service":"api","field":{"requires":{"index":0}}},"requested_path":"/private/link","inspection":{"resolved":null,"failure":"dangling-symlink","nodes":[{"path":"/private/link","kind":"symlink","target":target}],"complete":false}}],
                "omitted":0,"interrupted":false,"unavailable":0
            })
        };
        let mut left = diagnosis();
        left["incident"] = json!({"resources":resources("/private/target-a")});
        let mut right = diagnosis();
        right["incident"] = json!({"resources":resources("/private/target-b")});
        let result = compare(&left, &right, &Control::new()).unwrap();
        let change = result
            .changes
            .iter()
            .find(|c| c.category == "resource-observation")
            .unwrap();
        assert_eq!(change.status, "unknown");
    }

    #[test]
    fn test_compare_incident_trigger_hashes_data_and_preserves_times() {
        let trigger = |kind: &str| {
            json!({
                "schema_version":1,"run_id":"r","sequence":2,
                "timestamp":"2026-01-01T00:00:00Z","elapsed":{"secs":0," nanos":0},
                "service":"api","generation":1,"cause":null,"truncated":false,
                "data":{"type":kind}
            })
        };
        let mut left = diagnosis();
        left["incident"] = json!({"beginning":{"capture":{"observed_at":"2026-01-01T00:00:00Z","trigger":trigger("started")}}});
        let mut right = diagnosis();
        right["incident"] = json!({"beginning":{"capture":{"observed_at":"2026-01-01T00:00:01Z","trigger":trigger("exited")}}});
        let result = compare(&left, &right, &Control::new()).unwrap();
        let change = result
            .changes
            .iter()
            .find(|c| c.category == "incident-trigger")
            .unwrap();
        assert_eq!(change.status, "different");
        assert_eq!(
            change.observed_left.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert!(change.evidence.contains(&"not-shared-cause"));
        assert!(!serde_json::to_string(&result).unwrap().contains("started"));
    }

    #[test]
    fn test_compare_text_escapes_untrusted_service_names() {
        let mut right = diagnosis();
        right["recorded_state"] = json!({"api\u{1b}[2J": {"status":"failed"}});
        let result = compare(&diagnosis(), &right, &Control::new()).unwrap();
        let text = String::from_utf8(render(&result).unwrap()).unwrap();
        assert!(!text.contains('\u{1b}'));
        assert!(text.contains("\\u001b[2J"));
    }
}
