//! Parse only the recorded allowlist. Invalid suffixes never erase a valid prefix.
use super::super::{evidence, EndReason};
use crate::cli::{export::sanitize_event, instances::Identity};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::cli) struct Beginning {
    pub capture: evidence::Begin,
    pub configuration: evidence::Configuration,
    pub retention_evicted_this_recorder: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::cli) struct Evidence {
    pub id: String,
    pub artifact_schema_version: Option<u64>,
    pub collection_status: String,
    pub complete: bool,
    pub diagnostics: Vec<String>,
    pub beginning: Option<Beginning>,
    pub resources: Option<evidence::Resources>,
    pub end: Option<evidence::End>,
    pub declared_impact: String,
    pub current_state: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::cli) struct Summary {
    pub id: String,
    pub artifact_schema_version: Option<u64>,
    pub identity: Option<Identity>,
    pub service: Option<String>,
    pub generation: Option<u64>,
    pub trigger: Option<crate::core::events::query::EventKind>,
    pub observed_at: Option<DateTime<Utc>>,
    pub collection_status: String,
    pub complete: bool,
    pub diagnostics: Vec<String>,
}

impl Evidence {
    pub fn empty(id: String, diagnostic: &str) -> Self {
        Self {
            id,
            artifact_schema_version: None,
            collection_status: "unavailable".into(),
            complete: false,
            diagnostics: vec![diagnostic.into()],
            beginning: None,
            resources: None,
            end: None,
            declared_impact: "unknown-not-recorded-by-schema-1".into(),
            current_state: "not-inspected-historical-evidence-only".into(),
        }
    }
    pub fn summary(&self) -> Summary {
        let capture = self.beginning.as_ref().map(|b| &b.capture);
        Summary {
            id: self.id.clone(),
            artifact_schema_version: self.artifact_schema_version,
            identity: capture.map(|c| c.identity.clone()),
            service: capture.and_then(|c| c.trigger.service.clone()),
            generation: capture.and_then(|c| c.trigger.generation),
            trigger: capture.map(|c| c.trigger.data.kind()),
            observed_at: capture.map(|c| c.observed_at),
            collection_status: self.collection_status.clone(),
            complete: self.complete,
            diagnostics: self.diagnostics.clone(),
        }
    }
}

pub(super) fn parse(id: String, bytes: &[u8]) -> Evidence {
    let mut result = Evidence::empty(id, "missing-begin-record");
    let mut records = bytes.split_inclusive(|b| *b == b'\n');
    for index in 0..4 {
        let Some(line) = records.next() else { break };
        if index == 3 {
            result.diagnostics.push("extra-records-after-end".into());
            break;
        }
        if line.last() != Some(&b'\n') {
            result.diagnostics.push("unfinished-jsonl-tail".into());
            break;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            result.diagnostics.push("malformed-jsonl-record".into());
            break;
        };
        let record = value.get("record").and_then(|v| v.as_str());
        let data = value.get("data").cloned().unwrap_or_default();
        match (index, record) {
            (0, Some("begin")) => {
                result.artifact_schema_version = data
                    .pointer("/capture/schema_version")
                    .and_then(|v| v.as_u64());
                if !matches!(result.artifact_schema_version, Some(1 | 2)) {
                    result.diagnostics = vec!["unsupported-incident-schema-upgrade-devd".into()];
                    result.collection_status = "unsupported-schema".into();
                    return result;
                }
                let Ok(mut beginning) = serde_json::from_value::<Beginning>(data) else {
                    result.diagnostics.push("invalid-begin-record".into());
                    break;
                };
                if !valid_begin(&beginning, &result.id) {
                    result
                        .diagnostics
                        .push("invalid-identity-generation-or-context-limits".into());
                    break;
                }
                sanitize_event(&mut beginning.capture.trigger);
                for event in &mut beginning.capture.prior {
                    sanitize_event(event);
                }
                // These fields describe the schema contract, not arbitrary stored text.
                beginning.configuration.source =
                    "effective-runtime-config-observed-after-trigger".into();
                beginning.configuration.atomic = false;
                beginning.configuration.trigger_time_equivalence = "not-guaranteed".into();
                if result.artifact_schema_version == Some(2) {
                    result.declared_impact = "unknown-missing-relationship-evidence".into();
                    if beginning.configuration.observed_at != Some(beginning.capture.observed_at) {
                        result
                            .diagnostics
                            .push("invalid-configuration-observation-time".into());
                        break;
                    }
                } else {
                    beginning.configuration.observed_at = None;
                }
                result.diagnostics.clear();
                result.beginning = Some(beginning);
            }
            (1, Some("resources")) if result.beginning.is_some() => {
                // Decode the graph independently: a damaged optional relationship
                // section must not erase otherwise valid resource/end evidence.
                let mut data = data;
                let relationships = data.as_object_mut().and_then(|d| d.remove("relationships"));
                let Ok(mut resource) = serde_json::from_value::<evidence::Resources>(data) else {
                    result.diagnostics.push("invalid-resource-record".into());
                    break;
                };
                if resource.id != result.id
                    || resource.observations.len() > 8
                    || resource.observations.iter().any(|r| {
                        r.target.inspection.nodes.len() > 128
                            || r.target
                                .inspection
                                .failed_node
                                .is_some_and(|i| i >= r.target.inspection.nodes.len())
                            || Some(&r.target.selector.service)
                                != result
                                    .beginning
                                    .as_ref()
                                    .unwrap()
                                    .capture
                                    .trigger
                                    .service
                                    .as_ref()
                    })
                {
                    result
                        .diagnostics
                        .push("invalid-resource-identity-or-limits".into());
                    break;
                }
                resource.source = "fresh-post-trigger-metadata-and-link-inspection".into();
                resource.atomic = false;
                resource.trigger_time_equivalence = "not-guaranteed".into();
                for observation in &mut resource.observations {
                    observation.target.inspection.atomic = false;
                    observation
                        .target
                        .risk_hints
                        .retain(|s| s == "temporary-location-is-not-deletion-authorization");
                }
                if result.artifact_schema_version == Some(1) {
                    resource.relationships = None;
                } else if let Some(saved) = relationships.filter(|v| !v.is_null()) {
                    resource.relationships = serde_json::from_value(saved).ok();
                    let valid = resource.relationships.as_mut().is_some_and(|g| {
                        g.validate(
                            &result.beginning.as_ref().unwrap().configuration,
                            &resource.observations,
                        )
                    });
                    if !valid {
                        result
                            .diagnostics
                            .push("invalid-recorded-relationships".into());
                        resource.relationships = None;
                    } else {
                        let relationships = resource.relationships.as_mut().unwrap();
                        result.declared_impact = if relationships.complete() {
                            "recorded-declared-relationships"
                        } else {
                            "partial-recorded-declared-relationships"
                        }
                        .into();
                        if !relationships.complete() {
                            result
                                .diagnostics
                                .push("declared-relationship-coverage-incomplete".into());
                        }
                        if relationships.observed_to < relationships.observed_from
                            || relationships.observed_from < relationships.config_observed_at
                        {
                            result
                                .diagnostics
                                .push("relationship-observation-wall-clock-moved-backwards".into());
                        }
                    }
                } else {
                    result
                        .diagnostics
                        .push("missing-recorded-relationships".into());
                }
                result.resources = Some(resource);
            }
            (2, Some("end")) if result.resources.is_some() => {
                let Ok(mut end) = serde_json::from_value::<evidence::End>(data) else {
                    result.diagnostics.push("invalid-end-record".into());
                    break;
                };
                let begin = &result.beginning.as_ref().unwrap().capture;
                if end.id != result.id
                    || end.following.len() > 200
                    || encoded_size(&end.following) > 256 * 1024 + 1024
                    || !valid_events(
                        &end.following,
                        &begin.identity.run_id,
                        Some(begin.trigger.sequence),
                        None,
                    )
                {
                    result
                        .diagnostics
                        .push("invalid-end-identity-or-event-order".into());
                    break;
                }
                for event in &mut end.following {
                    sanitize_event(event);
                }
                result.end = Some(end);
            }
            _ => {
                result.diagnostics.push("unexpected-record-order".into());
                break;
            }
        }
    }
    if result.beginning.is_none() {
        return result;
    }
    if result.resources.is_none() {
        result
            .diagnostics
            .push("missing-resource-observations".into());
    }
    if result.end.is_none() {
        result
            .diagnostics
            .push("missing-end-open-or-interrupted-capture".into());
    }
    result.collection_status = match result.end.as_ref().map(|e| e.reason) {
        Some(EndReason::Deadline) => "window-ended",
        Some(EndReason::Cancelled | EndReason::Disconnected) => "interrupted",
        Some(_) => "ended-early",
        None => "incomplete",
    }
    .into();
    let begin = result.beginning.as_ref().unwrap();
    if begin.capture.prior_omitted > 0
        || begin.capture.statistics.skipped_events > 0
        || begin.capture.statistics.dropped_triggers > 0
    {
        result
            .diagnostics
            .push("prior-context-or-recorder-coverage-gap".into());
    }
    if begin.retention_evicted_this_recorder > 0 {
        result
            .diagnostics
            .push("older-incidents-evicted-by-this-recorder".into());
    }
    if begin.configuration.omitted_services > 0
        || begin
            .configuration
            .services
            .iter()
            .any(|s| s.omitted_dependencies > 0)
    {
        result
            .diagnostics
            .push("configuration-summary-incomplete".into());
    }
    if result
        .resources
        .as_ref()
        .is_some_and(|r| r.observed_to < r.observed_from)
        || result
            .end
            .as_ref()
            .is_some_and(|end| end.observed_at < begin.capture.observed_at)
    {
        result
            .diagnostics
            .push("observation-wall-clock-moved-backwards".into());
    }
    if result.resources.as_ref().is_some_and(|r| {
        r.omitted > 0
            || r.unavailable > 0
            || r.interrupted
            || r.observations.iter().any(|r| !r.target.inspection.complete)
    }) {
        result
            .diagnostics
            .push("resource-observation-coverage-incomplete".into());
    }
    if let Some(end) = &result.end {
        if end.following_omitted != Some(0)
            || end.skipped_events != Some(0)
            || end
                .statistics
                .as_ref()
                .is_none_or(|s| s.dropped_triggers > 0)
        {
            result
                .diagnostics
                .push("following-context-or-recorder-coverage-gap".into());
        }
        if end.reason != EndReason::Deadline {
            result
                .diagnostics
                .push("follow-up-window-ended-early".into());
        }
    }
    if begin.capture.trigger.truncated
        || begin
            .capture
            .prior
            .iter()
            .chain(result.end.iter().flat_map(|e| &e.following))
            .any(|e| e.truncated)
    {
        result.diagnostics.push("truncated-event-content".into());
    }
    result.complete =
        result.end.is_some() && result.resources.is_some() && result.diagnostics.is_empty();
    result
}

fn valid_begin(begin: &Beginning, id: &str) -> bool {
    let capture = &begin.capture;
    (capture.schema_version != 2 || {
        let mut names = std::collections::BTreeSet::new();
        begin.configuration.services.iter().all(|s| {
            s.name.len() <= 4096
                && names.insert(&s.name)
                && s.dependencies.iter().all(|d| d.service.len() <= 4096)
        })
    }) && encoded_size(&capture.identity) <= 64 * 1024
        && encoded_size(&capture.prior) <= 256 * 1024 + 1024
        && encoded_size(&begin.configuration) <= 128 * 1024 + 1024
        && capture.id == id
        && capture.identity.schema_version == 1
        && uuid::Uuid::parse_str(&capture.identity.run_id).is_ok()
        && capture.trigger.run_id == capture.identity.run_id
        && capture.trigger.service.is_some()
        && capture.prior.len() <= 200
        && begin.configuration.services.len() <= 256
        && begin
            .configuration
            .services
            .iter()
            .all(|s| s.dependencies.len() <= 64)
        && valid_events(
            std::slice::from_ref(&capture.trigger),
            &capture.identity.run_id,
            None,
            None,
        )
        && valid_events(
            &capture.prior,
            &capture.identity.run_id,
            None,
            Some(capture.trigger.sequence),
        )
}

fn valid_events(
    events: &[crate::core::events::LifecycleEvent],
    run: &str,
    lower: Option<u64>,
    upper: Option<u64>,
) -> bool {
    let mut previous = lower;
    for event in events {
        if event.run_id != run
            || previous.is_some_and(|n| n >= event.sequence)
            || upper.is_some_and(|n| event.sequence >= n)
            || event.generation.is_some_and(|n| n > event.sequence)
            || event.cause.is_some_and(|n| n >= event.sequence)
        {
            return false;
        }
        previous = Some(event.sequence);
    }
    true
}

fn encoded_size(value: &impl Serialize) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |b| b.len())
}
