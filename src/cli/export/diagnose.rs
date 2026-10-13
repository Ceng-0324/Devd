//! Transferred reports are data, never a source of paths to inspect or actions.
#[cfg(test)]
mod tests;
mod validation;

use super::output::{self, CancelOnDrop, Control, DEADLINE, MAX_REPORT_BYTES};
use crate::{
    cli::{incidents::query, instances::Identity},
    core::{diagnostics::ExplainReport, events::query::EventBatch},
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(clap::Args)]
pub(in crate::cli) struct Args {
    /// Exported JSON report (schemas 1–3); no original project is needed.
    report: PathBuf,
    #[arg(long)]
    json: bool,
}

#[derive(Serialize)]
struct Diagnosis {
    schema_version: u16,
    source: &'static str,
    input_schema_version: u16,
    captured_from: DateTime<Utc>,
    captured_to: DateTime<Utc>,
    capture_reported_stable: bool,
    current_health: &'static str,
    recorded_state_attribution: &'static str,
    stored_run_coverage: Option<RunCoverage>,
    historical_identity: Option<Identity>,
    recorded_state: BTreeMap<String, super::SafeServiceState>,
    timeline: EventBatch,
    explanations: BTreeMap<String, ExplainReport>,
    incident: Option<query::Evidence>,
    log_attachments: Vec<LogSummary>,
    limitations: Vec<&'static str>,
}

#[derive(Serialize)]
struct RunCoverage {
    begin_reported_retained: bool,
    end_reported_retained: bool,
    state_availability: String,
}

#[derive(Serialize)]
struct LogSummary {
    location: &'static str,
    entries: usize,
    attribution: &'static str,
}

pub(in crate::cli) async fn run(args: Args) -> Result<()> {
    let mut signals = crate::platform::shutdown::Shutdown::new(false)?;
    let control = Control::new();
    let _cancel = CancelOnDrop(control.clone());
    let c = control.clone();
    let work = tokio::task::spawn_blocking(move || {
        let bytes = output::read_regular(&args.report, &c, MAX_REPORT_BYTES)?.bytes;
        let diagnosis = validation::parse(&bytes, &c)?;
        c.check()?;
        let text = if args.json {
            output::encode(&diagnosis)?
        } else {
            render(&diagnosis)?
        };
        c.check()?;
        Ok::<_, anyhow::Error>(text)
    });
    let bytes = tokio::select! {
        biased;
        _ = signals.recv() => bail!("diagnosis cancelled"),
        _ = tokio::time::sleep(DEADLINE) => bail!("diagnosis exceeded 30s"),
        result = work => result.context("diagnostic reader failed")??,
    };
    let mut out = crate::cli::stdout::Stdout::new()?;
    write(&mut out, &bytes, signals.recv()).await
}

/// Load one transferred report for adapters that need structured tool output.
/// The same bounded reader and validator as the CLI is used; no project paths
/// or supervisor state are consulted.
pub(in crate::cli) async fn load_json(path: &Path) -> Result<serde_json::Value> {
    let control = Control::new();
    let _cancel = CancelOnDrop(control.clone());
    let work = tokio::task::spawn_blocking({
        let path = path.to_owned();
        let control = control.clone();
        move || {
            let bytes = output::read_regular(&path, &control, MAX_REPORT_BYTES)?.bytes;
            let diagnosis = validation::parse(&bytes, &control)?;
            let bytes = output::encode(&diagnosis)?;
            control.check()?;
            let value =
                serde_json::from_slice(&bytes).context("diagnostic encoding was not JSON")?;
            control.check()?;
            Ok::<_, anyhow::Error>(value)
        }
    });
    tokio::time::timeout(DEADLINE, work)
        .await
        .context("diagnosis exceeded 30s")?
        .context("diagnostic reader failed")?
}

async fn write(
    out: &mut (impl tokio::io::AsyncWrite + Unpin),
    bytes: &[u8],
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    tokio::select! {
        biased;
        _ = cancelled => bail!("diagnostic output cancelled; output may be incomplete"),
        result = tokio::time::timeout(crate::cli::protocol::IO_TIMEOUT, async {
            out.write_all(bytes).await?;
            out.write_all(b"\n").await?;
            out.flush().await
        }) => result.context("diagnostic output timed out; output may be incomplete")??,
    }
    Ok(())
}

fn render(d: &Diagnosis) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut text = output::BoundedBytes(Vec::new());
    writeln!(
        text,
        "Offline diagnosis — historical evidence; current health is not inspected"
    )?;
    writeln!(
        text,
        "Input schema: {}  Captured: {} … {}",
        d.input_schema_version, d.captured_from, d.captured_to
    )?;
    writeln!(
        text,
        "Capture reported stable: {}",
        d.capture_reported_stable
    )?;
    for limitation in &d.limitations {
        writeln!(text, "Note: {limitation}")?;
    }
    writeln!(
        text,
        "Historical identity: {}",
        serde_json::to_string(&d.historical_identity)?
    )?;
    writeln!(
        text,
        "Retained-run coverage: {}",
        serde_json::to_string(&d.stored_run_coverage)?
    )?;
    writeln!(text, "Recorded state ({})", d.recorded_state_attribution)?;
    for (name, state) in &d.recorded_state {
        writeln!(
            text,
            "  {}: {}",
            serde_json::to_string(name)?,
            serde_json::to_string(state)?
        )?;
    }
    writeln!(
        text,
        "Deterministic explanations ({}):",
        if d.incident.is_some() {
            "incident trigger generation"
        } else {
            "latest retained generation"
        }
    )?;
    for e in d.explanations.values() {
        writeln!(
            text,
            "  {} generation={:?} conclusion={} evidence_complete={}",
            serde_json::to_string(&e.service)?,
            e.generation,
            serde_json::to_string(&e.conclusion)?,
            e.complete
        )?;
        writeln!(text, "    {}", serde_json::to_string(&e.summary)?)?;
        for detail in &e.details {
            writeln!(text, "    {}", serde_json::to_string(detail)?)?;
        }
        for reference in &e.evidence {
            writeln!(text, "    evidence: {}", serde_json::to_string(reference)?)?;
        }
        for step in &e.next_steps {
            writeln!(
                text,
                "    suggestion (not executed): {}",
                serde_json::to_string(step)?
            )?;
        }
    }
    writeln!(
        text,
        "Timeline (sequence order; timestamps are observations):"
    )?;
    writeln!(
        text,
        "  context={} first_available={:?} cursor={}",
        serde_json::to_string(&d.timeline.context)?,
        d.timeline.first_available,
        serde_json::to_string(&d.timeline.cursor)?
    )?;
    for event in &d.timeline.entries {
        writeln!(
            text,
            "  #{} {}",
            event.sequence,
            serde_json::to_string(event)?
        )?;
    }
    writeln!(
        text,
        "Gaps (additional omitted={}): {}",
        d.timeline.omitted_gaps,
        serde_json::to_string(&d.timeline.gaps)?
    )?;
    if let Some(e) = &d.incident {
        writeln!(
            text,
            "Incident: {}  {}  complete={}\nDeclared impact: {}",
            e.id, e.collection_status, e.complete, e.declared_impact
        )?;
        if let Some(r) = &e.resources {
            if let Some(g) = &r.relationships {
                let trigger = e
                    .beginning
                    .as_ref()
                    .and_then(|b| b.capture.trigger.service.as_deref())
                    .unwrap_or("");
                text.write_all(g.render(trigger, &r.observations).as_bytes())?;
            }
        }
        writeln!(text, "Recorded incident evidence (paths may be sensitive):")?;
        serde_json::to_writer_pretty(&mut text, e)?;
        writeln!(text)?;
    }
    writeln!(
        text,
        "Log attachment metadata (contents not displayed): {}",
        serde_json::to_string(&d.log_attachments)?
    )?;
    // JSON already quotes C0 controls. Escape C1 and direction controls too,
    // which JSON otherwise permits verbatim and terminals may interpret.
    let rendered = String::from_utf8(text.0)?;
    let mut safe = output::BoundedBytes(Vec::new());
    for c in rendered.chars() {
        if matches!(c, '\u{7f}'..='\u{9f}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            write!(safe, "\\u{:04x}", c as u32)?;
        } else {
            write!(safe, "{c}")?;
        }
    }
    Ok(safe.0)
}
