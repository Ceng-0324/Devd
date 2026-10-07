use super::protocol::{self, Request, Response};
use crate::core::{
    diagnostics::ExplainReport,
    events::{
        query::{EventFilter, EventQuery},
        storage::read_stored,
    },
};
use anyhow::{bail, Context, Result};
use std::path::Path;

#[derive(clap::Args)]
pub(super) struct Args {
    /// Service whose latest lifecycle facts should be explained.
    pub(super) service: String,
    /// Print the versioned report as JSON.
    #[arg(long)]
    pub(super) json: bool,
    /// Explain retained facts after the supervisor has stopped.
    #[arg(long)]
    pub(super) stored: bool,
}

pub(super) async fn run(args: Args, socket: &Path, state_dir: &Path) -> Result<()> {
    let report = if args.stored {
        let directory = state_dir.join("events");
        let query = full_query();
        let batch = tokio::task::spawn_blocking(move || read_stored(&directory, &query))
            .await
            .context("stored event reader task failed")?
            .context(
                "cannot read stored events; enable persistence at startup and query after shutdown",
            )?;
        crate::core::diagnostics::explain(&args.service, None, batch)
    } else {
        let Response::Explain(report) = protocol::request(
            socket,
            Request::Explain {
                service: args.service,
            },
        )
        .await?
        else {
            bail!("unexpected explain response");
        };
        report
    };
    super::output(&render(&report, args.json)?)
}

fn full_query() -> EventQuery {
    EventQuery {
        filter: EventFilter::default(),
        tail: 1000,
        cursor: None,
    }
}

fn render(report: &ExplainReport, json: bool) -> Result<String> {
    if json {
        return Ok(format!("{}\n", serde_json::to_string_pretty(report)?));
    }
    let mut text = format!(
        "{} [{}] source={:?} complete={}\n",
        report.summary, report.service, report.source, report.complete
    );
    if let Some(status) = report.status {
        text.push_str(&format!("状态: {status:?}\n"));
    }
    for detail in &report.details {
        text.push_str(&format!("原因: {detail}\n"));
    }
    for evidence in &report.evidence {
        text.push_str(&format!(
            "证据 #{} {}: {}\n",
            evidence.sequence,
            serde_json::to_string(&evidence.event_type)?,
            evidence.detail
        ));
    }
    if !report.gaps.is_empty() || report.omitted_gaps > 0 {
        text.push_str("数据缺口:\n");
        for gap in &report.gaps {
            text.push_str(&format!("  {}\n", serde_json::to_string(gap)?));
        }
        if report.omitted_gaps > 0 {
            text.push_str(&format!("  省略 {} 个更早的缺口\n", report.omitted_gaps));
        }
    }
    text.push_str("下一步:\n");
    for step in &report.next_steps {
        text.push_str(&format!("  {step}\n"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        diagnostics::ExplainConclusion,
        events::{query::EventQuery, EventData, EventRecorder},
        service_manager::{RuntimeSnapshot, ServiceSnapshot, ServiceState},
    };

    #[test]
    fn test_explain_text_and_json_share_report_fields() {
        let recorder = EventRecorder::new("state".into(), None);
        recorder.record(Some("api"), None, None, EventData::Starting);
        let snapshot = RuntimeSnapshot {
            supervisor_pid: 1,
            event_run_id: Some(recorder.run_id().into()),
            services: [(
                "api".into(),
                ServiceSnapshot {
                    status: ServiceState::Running,
                    ..Default::default()
                },
            )]
            .into(),
        };
        let batch = EventQuery {
            filter: Default::default(),
            tail: 1000,
            cursor: None,
        }
        .select(recorder.history().snapshot())
        .unwrap();
        let report = crate::core::diagnostics::explain("api", Some(&snapshot), batch);
        assert_eq!(report.conclusion, ExplainConclusion::Running);
        assert!(render(&report, false).unwrap().contains("api"));
        assert!(render(&report, true)
            .unwrap()
            .contains("\"conclusion\": \"running\""));
    }
}
