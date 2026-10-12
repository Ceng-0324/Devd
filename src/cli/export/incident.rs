//! Incident reports share safe event/explanation fields; all evidence stays historical.
use super::{output::Control, stored::Selection, Report};
use crate::{
    cli::{
        incidents::query,
        protocol::{self, Request, Response},
    },
    core::events::{
        query::{EventBatch, EventCursor, EventGap, EventSource},
        EventContext,
    },
};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Evidence {
    recorded: query::Report,
    #[serde(skip_serializing_if = "Option::is_none")]
    unassigned_stored_logs: Option<super::stored::LogAttachment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_run_logs: Option<Vec<crate::logging::LogEntry>>,
    log_attribution: String,
    limitations: Vec<String>,
}

pub(super) async fn capture(
    id: String,
    stored: bool,
    include_logs: bool,
    socket: &Path,
    selection: Selection,
    control: &Control,
) -> Result<Report> {
    let from = Utc::now();
    let recorded = if stored {
        let c = control.clone();
        let config = selection.config.clone();
        let state = selection.state_dir.clone();
        let profile = selection.profile.clone();
        tokio::task::spawn_blocking(move || {
            query::offline(Some(id), &config, &state, profile.as_deref(), &c)
        })
        .await
        .context("incident read task failed")??
    } else {
        query::live(Some(id), socket).await?
    };
    control.check()?;
    let evidence = recorded
        .incident
        .as_ref()
        .context("missing incident evidence")?;
    let (events, explanations) = describe(evidence);
    let (unassigned_stored_logs, current_run_logs) = if !include_logs {
        (None, None)
    } else if stored {
        let c = control.clone();
        let state = selection.state_dir;
        let logs = tokio::task::spawn_blocking(move || {
            let _lease = crate::platform::files::FileLock::acquire(
                crate::platform::files::open_regular(
                    &state.join("services.json.lock"),
                    false,
                    true,
                )?,
                true,
            )
            .context("state is in use; stop supervisor before reading stored logs")?;
            super::stored::read_logs(&state.join("logs"), &c)
        })
        .await
        .context("stored log attachment failed")??;
        (Some(logs), None)
    } else {
        let Response::Logs(logs) = protocol::request(
            socket,
            Request::Scoped {
                instance_id: recorded.selected_instance_id.clone(),
                run_id: recorded
                    .selected_run
                    .clone()
                    .context("missing run selection")?,
                request: Box::new(Request::Logs {
                    service: None,
                    tail: super::LOG_LIMIT,
                    filter: Default::default(),
                }),
            },
        )
        .await?
        else {
            bail!("unexpected log response or supervisor run changed")
        };
        (None, Some(logs))
    };
    control.check()?;
    Ok(Report {
        schema_version: 3,
        identity: None,
        source: EventSource::Stored,
        stored: None,
        captured_from: from,
        captured_to: Utc::now(),
        stable_during_capture: false,
        event_watermark_after: events.cursor.as_ref().map_or(0, |c| c.next_sequence),
        omitted_fields: super::omitted_fields(),
        state: BTreeMap::new(),
        events,
        explanations,
        logs: None,
        log_tail_limit: None,
        incident: Some(Evidence {
            recorded,
            unassigned_stored_logs,
            current_run_logs,
            log_attribution: if !include_logs {
                "not-requested"
            } else if stored {
                "unknown-no-run-id-in-stored-logs"
            } else {
                "selected-current-run-not-incident-window"
            }
            .into(),
            limitations: super::super::incidents::evidence::LIMITATIONS
                .iter()
                .map(|s| (*s).into())
                .collect(),
        }),
    })
}

/// Pure historical projection shared by export and transferred-report diagnosis.
pub(super) fn describe(
    evidence: &query::Evidence,
) -> (
    EventBatch,
    BTreeMap<String, crate::core::diagnostics::ExplainReport>,
) {
    let mut events = EventBatch {
        schema_version: 1,
        source: EventSource::Stored,
        persistence: None,
        context: None,
        first_available: None,
        cursor: None,
        gaps: vec![],
        omitted_gaps: 0,
        entries: vec![],
    };
    let mut explanations = BTreeMap::new();
    if let Some(begin) = &evidence.beginning {
        let b = &begin.capture;
        events.context = Some(EventContext {
            run_id: b.identity.run_id.clone(),
            state_path: b.identity.state_dir.join("services.json"),
            profile: b.identity.profile.clone(),
        });
        events.entries.extend(b.prior.iter().cloned());
        events.entries.push(b.trigger.clone());
        if let Some(end) = &evidence.end {
            events.entries.extend(end.following.iter().cloned());
        }
        events.first_available = events.entries.first().map(|e| e.sequence);
        events.cursor = events
            .entries
            .last()
            .and_then(|e| e.sequence.checked_add(1))
            .map(|next_sequence| EventCursor {
                run_id: b.identity.run_id.clone(),
                next_sequence,
            });
        // A bounded incident window never certifies a complete run's history.
        events.gaps.push(EventGap::IncompleteRun {
            run_id: b.identity.run_id.clone(),
        });
        let mut generation = events.clone();
        generation
            .entries
            .retain(|e| e.service == b.trigger.service && e.generation == b.trigger.generation);
        if let Some(name) = &b.trigger.service {
            let mut explanation = crate::core::diagnostics::explain(name, None, &generation);
            explanation.complete = false;
            explanation
                .details
                .push("历史事故窗口中的记录；不代表当前健康，不推断未记录的资源影响或根因".into());
            explanations.insert(name.clone(), explanation);
        }
    }
    (events, explanations)
}
