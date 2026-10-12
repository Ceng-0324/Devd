//! One bounded, read-only use case for live writer-owned and offline locked evidence.
mod model;
use super::storage;
use crate::{
    cli::{
        export::output::{CancelOnDrop, Control, DEADLINE},
        instances,
        protocol::{self, Request, Response},
    },
    core::state_store::StateStore,
    platform::files,
};
use anyhow::{bail, Context, Result};
#[cfg(windows)]
use cap_fs_ext::MetadataExt;
use cap_std::fs::Dir;
#[cfg(unix)]
use cap_std::fs::MetadataExt;
pub(in crate::cli) use model::Evidence;
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(clap::Args)]
pub(in crate::cli) struct Args {
    #[command(subcommand)]
    action: Action,
    /// Read recorded files after the supervisor stops; never inspect current resources.
    #[arg(long, global = true)]
    stored: bool,
    #[arg(long, global = true)]
    json: bool,
}
#[derive(clap::Subcommand)]
enum Action {
    /// List retained incidents (current run online, all retained runs offline).
    List,
    /// Inspect a recorded incident by its complete UUID.
    Show {
        #[arg(value_parser = parse_id)]
        id: String,
    },
}
pub(in crate::cli) fn parse_id(text: &str) -> Result<String, String> {
    uuid::Uuid::parse_str(text)
        .map(|id| id.to_string())
        .map_err(|_| "incident ID must be a UUID".into())
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::cli) struct Report {
    pub schema_version: u16,
    pub source: String,
    pub selected_instance_id: String,
    pub selected_run: Option<String>,
    pub atomic: bool,
    pub complete: bool,
    pub diagnostics: Vec<String>,
    pub entries: Vec<model::Summary>,
    pub incident: Option<Evidence>,
}

/// Revalidate transferred evidence through the same prefix parser as retained
/// artifacts. Derived summaries and coverage claims in the report are ignored.
pub(in crate::cli) fn import(value: &serde_json::Value) -> Result<Evidence> {
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .context("missing incident ID")?;
    let id = parse_id(id).map_err(anyhow::Error::msg)?;
    if let Some(version) = value
        .get("artifact_schema_version")
        .and_then(|v| v.as_u64())
    {
        if !matches!(version, 1 | 2) {
            let mut evidence = Evidence::empty(id, "unsupported-incident-schema-upgrade-devd");
            evidence.artifact_schema_version = Some(version);
            evidence.collection_status = "unsupported-schema".into();
            evidence.declared_impact = "unknown-unsupported-incident-schema".into();
            return Ok(evidence);
        }
    }
    let mut bytes = Vec::new();
    for (field, record) in [
        ("beginning", "begin"),
        ("resources", "resources"),
        ("end", "end"),
    ] {
        if let Some(data) = value.get(field).filter(|v| !v.is_null()) {
            serde_json::to_writer(
                &mut bytes,
                &serde_json::json!({"record":record, "data":data}),
            )?;
            bytes.push(b'\n');
        }
    }
    // Escaping a transferred JSON object back to the artifact representation
    // must still respect the recorder's original bound.
    if bytes.len() > storage::MAX_BYTES {
        bail!("incident evidence exceeds 2 MiB");
    }
    let mut evidence = model::parse(id, &bytes);
    // Reader-only failures (e.g. a discarded JSONL tail) cannot be reproduced
    // from the surviving records. Preserve them with explicit attribution.
    if let Some(reported) = value.get("diagnostics") {
        let reported = reported
            .as_array()
            .context("invalid incident diagnostics")?;
        if reported.len() > 64 {
            bail!("incident diagnostic count exceeds limit");
        }
        for diagnostic in reported {
            let diagnostic = diagnostic.as_str().context("invalid incident diagnostic")?;
            if diagnostic.len() > 256 {
                bail!("incident diagnostic exceeds size limit");
            }
            if !evidence.diagnostics.iter().any(|d| d == diagnostic) {
                evidence
                    .diagnostics
                    .push(format!("imported-reader-reported: {diagnostic}"));
            }
        }
        if !reported.is_empty() {
            evidence.complete = false;
        }
    }
    // A previous reader may have discarded a damaged suffix/optional graph.
    // Do not turn that loss of evidence into a newly complete collection.
    if value.get("complete").and_then(|v| v.as_bool()) != Some(true) && evidence.complete {
        evidence.complete = false;
        evidence
            .diagnostics
            .push("imported-collection-reported-incomplete".into());
    }
    Ok(evidence)
}

#[derive(Clone)]
pub(in crate::cli) struct Reader {
    directory: Arc<Dir>,
    gate: Arc<Mutex<()>>,
    _lock: Arc<files::FileLock>,
    _live_lease: Option<Arc<StateStore>>,
    _offline_lease: Option<Arc<files::FileLock>>,
}
impl Reader {
    pub(super) fn live(
        directory: Arc<Dir>,
        gate: Arc<Mutex<()>>,
        lock: Arc<files::FileLock>,
        lease: Arc<StateStore>,
    ) -> Self {
        Self {
            directory,
            gate,
            _lock: lock,
            _live_lease: Some(lease),
            _offline_lease: None,
        }
    }
    fn offline(state: &Path) -> Result<Option<Self>> {
        let meta = std::fs::symlink_metadata(state)?;
        if !meta.is_dir() || files::is_link(&meta) {
            bail!("unsafe incident state directory");
        }
        let parent = Dir::open_ambient_dir(state, cap_std::ambient_authority())?;
        let lease = files::FileLock::acquire(
            storage::read_file(&parent, "services.json.lock")?.into_std(),
            true,
        )
        .context("state is in use; stop the supervisor before --stored")?;
        match parent.symlink_metadata("incidents") {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        let directory = Arc::new(storage::open_directory(&parent, "incidents")?);
        let lock =
            files::FileLock::acquire(storage::read_file(&directory, ".lock")?.into_std(), true)
                .context("incident writer is busy; retry after recording stops")?;
        Ok(Some(Self {
            directory,
            gate: Arc::new(Mutex::new(())),
            _lock: Arc::new(lock),
            _live_lease: None,
            _offline_lease: Some(Arc::new(lease)),
        }))
    }
    pub(in crate::cli) fn read(
        &self,
        id: Option<String>,
        instance: String,
        run: Option<String>,
        control: &Control,
    ) -> Result<Report> {
        // Waiting for a writer OS call must not consume an unbounded number of
        // tasks; the live adapter owns at most two permits until workers finish.
        let _guard = loop {
            control.check()?;
            match self.gate.try_lock() {
                Ok(g) => break g,
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(_) => bail!("incident storage mutex poisoned"),
            }
        };
        let mut report = empty_report(instance, run);
        if let Some(id) = id {
            let id = parse_id(&id).map_err(anyhow::Error::msg)?;
            let evidence = self.read_one(&id, control, || {})?;
            if evidence
                .diagnostics
                .iter()
                .any(|d| d == "artifact-not-found-may-have-been-evicted")
            {
                bail!("incident not found; it may have been evicted by retention");
            }
            if let Some(begin) = &evidence.beginning {
                check_scope(&begin.capture.identity, &report)?;
            }
            report.complete = evidence.complete;
            report.entries.push(evidence.summary());
            report.incident = Some(evidence);
        } else {
            let mut names = vec![];
            for (n, entry) in self.directory.entries()?.enumerate() {
                control.check()?;
                if n == 256 {
                    bail!("incident directory exceeds scan limit of 256 entries");
                }
                let entry = entry?;
                if let Some(name) = entry
                    .file_name()
                    .to_str()
                    .filter(|n| storage::managed_name(n))
                {
                    names.push(name.to_owned());
                }
            }
            names.sort();
            if names.len() > 20 {
                report
                    .diagnostics
                    .push("artifact-read-limit-20-reached-selection-by-filename".into());
                names.truncate(20);
            }
            for name in names {
                control.check()?;
                let id = name.strip_suffix(".jsonl").unwrap();
                let evidence = self.read_one(id, control, || {})?;
                if let Some(begin) = &evidence.beginning {
                    if check_scope(&begin.capture.identity, &report).is_err() {
                        continue;
                    }
                } else {
                    report
                        .diagnostics
                        .push(format!("{id}: identity-unavailable-scope-unknown"));
                }
                report.entries.push(evidence.summary());
            }
            report
                .entries
                .sort_by(|a, b| b.observed_at.cmp(&a.observed_at).then(a.id.cmp(&b.id)));
            report.complete =
                report.diagnostics.is_empty() && report.entries.iter().all(|e| e.complete);
        }
        control.check()?;
        Ok(report)
    }
    fn read_one(
        &self,
        id: &str,
        control: &Control,
        before_recheck: impl FnOnce(),
    ) -> Result<Evidence> {
        let name = format!("{id}.jsonl");
        let file = match storage::read_file(&self.directory, &name) {
            Ok(f) => f,
            Err(e) => {
                control.check()?;
                return Ok(Evidence::empty(
                    id.into(),
                    if e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                    {
                        "artifact-not-found-may-have-been-evicted"
                    } else {
                        "unsafe-or-unreadable-artifact"
                    },
                ));
            }
        };
        let before = file.metadata()?;
        if before.len() > storage::MAX_BYTES as u64 {
            return Ok(Evidence::empty(id.into(), "artifact-exceeds-2-MiB"));
        }
        let mut bytes = Vec::new();
        let mut file = file;
        let mut chunk = [0u8; 64 * 1024];
        loop {
            control.check()?;
            let count = file.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            if bytes.len() + count > storage::MAX_BYTES {
                return Ok(Evidence::empty(id.into(), "artifact-grew-past-2-MiB"));
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
        before_recheck();
        control.check()?;
        let same = |m: &cap_std::fs::Metadata| {
            (m.dev(), m.ino(), m.len(), m.modified().ok())
                == (
                    before.dev(),
                    before.ino(),
                    before.len(),
                    before.modified().ok(),
                )
        };
        if !same(&file.metadata()?)
            || !storage::read_file(&self.directory, &name)
                .ok()
                .and_then(|f| f.metadata().ok())
                .is_some_and(|m| same(&m))
        {
            return Ok(Evidence::empty(
                id.into(),
                "artifact-changed-during-read-retry",
            ));
        }
        control.check()?;
        Ok(model::parse(id.into(), &bytes))
    }
}
fn check_scope(identity: &instances::Identity, report: &Report) -> Result<()> {
    if identity.instance_id != report.selected_instance_id
        || report
            .selected_run
            .as_ref()
            .is_some_and(|r| r != &identity.run_id)
    {
        bail!("incident belongs to a different instance or run; use the matching instance and --stored after shutdown");
    }
    Ok(())
}
fn empty_report(instance: String, run: Option<String>) -> Report {
    Report {
        schema_version: 1,
        source: if run.is_some() {
            "live-recorder-recorded-evidence"
        } else {
            "stored-recorded-evidence"
        }
        .into(),
        selected_instance_id: instance,
        selected_run: run,
        atomic: false,
        complete: true,
        diagnostics: vec![],
        entries: vec![],
        incident: None,
    }
}
pub(in crate::cli) fn offline(
    id: Option<String>,
    config: &Path,
    state: &Path,
    profile: Option<&str>,
    control: &Control,
) -> Result<Report> {
    control.check()?;
    let instance = instances::instance_id(config, &std::fs::canonicalize(state)?, profile)?;
    match Reader::offline(state)? {
        Some(reader) => reader.read(id, instance, None, control),
        None if id.is_some() => bail!("incident storage is missing; enable start --record-incidents before reproducing the failure"),
        None => { let mut r = empty_report(instance, None); r.diagnostics.push("incident-storage-missing".into()); r.complete = false; Ok(r) },
    }
}
pub(in crate::cli) async fn live(id: Option<String>, socket: &Path) -> Result<Report> {
    let Response::Identity(identity) = protocol::request(socket, Request::Identity).await? else {
        bail!("unexpected identity response")
    };
    let Response::Incidents(report) = protocol::request(
        socket,
        Request::Scoped {
            instance_id: identity.instance_id.clone(),
            run_id: identity.run_id.clone(),
            request: Box::new(Request::Incidents { id }),
        },
    )
    .await?
    else {
        bail!("unexpected incident response or instance changed")
    };
    if report.selected_instance_id != identity.instance_id
        || report.selected_run.as_ref() != Some(&identity.run_id)
    {
        bail!("incident response scope mismatch")
    }
    Ok(*report)
}

pub(in crate::cli) async fn run(
    args: Args,
    socket: &Path,
    config: &Path,
    state: &Path,
    profile: Option<&str>,
) -> Result<()> {
    let mut signals = crate::platform::shutdown::Shutdown::new(false)?;
    let control = Control::new();
    let _cancel = CancelOnDrop(control.clone());
    let id = match args.action {
        Action::List => None,
        Action::Show { id } => Some(id),
    };
    let work = async {
        if args.stored {
            let (config, state, profile, control) = (
                config.to_owned(),
                state.to_owned(),
                profile.map(str::to_owned),
                control.clone(),
            );
            tokio::task::spawn_blocking(move || {
                offline(id, &config, &state, profile.as_deref(), &control)
            })
            .await
            .context("incident reader failed")?
        } else {
            live(id, socket).await
        }
    };
    let report = tokio::select! {
        biased;
        _ = signals.recv() => bail!("incident query cancelled"),
        _ = tokio::time::sleep(DEADLINE) => bail!("incident query exceeded 30s"),
        result = work => result?,
    };
    let text = if args.json {
        serde_json::to_string_pretty(&report)?
    } else {
        render(&report)
    };
    if text.len() > 16 * 1024 * 1024 {
        bail!("incident output exceeds 16 MiB")
    }
    let mut out = crate::cli::stdout::Stdout::new()?;
    write_report(&mut out, &text, signals.recv()).await
}

async fn write_report(
    out: &mut (impl tokio::io::AsyncWrite + Unpin),
    text: &str,
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    tokio::select! {
        biased;
        _ = cancelled => bail!("incident output cancelled; output may be incomplete"),
        result = tokio::time::timeout(protocol::IO_TIMEOUT, async {out.write_all(text.as_bytes()).await?; out.write_all(b"\n").await?; out.flush().await}) => result.context("incident output timed out; output may be incomplete")??,
    }
    Ok(())
}
fn render(report: &Report) -> String {
    use std::fmt::Write;
    let mut text = format!(
        "Recorded incidents ({}) — current health is not inspected\nInstance: {}\n",
        report.source, report.selected_instance_id
    );
    if report.entries.is_empty() {
        text.push_str("No retained incidents in this selection.\n");
    }
    for item in &report.entries {
        writeln!(
            text,
            "{}  observed={}  run={}  service={}  generation={}  trigger={}  {}",
            item.id,
            item.observed_at
                .map_or_else(|| "unknown".into(), |time| time.to_rfc3339()),
            item.identity
                .as_ref()
                .map_or("unknown", |i| i.run_id.as_str()),
            item.service.as_ref().map_or_else(
                || "unknown".into(),
                |name| serde_json::to_string(name).expect("service name"),
            ),
            item.generation
                .map_or_else(|| "unknown".into(), |generation| generation.to_string()),
            item.trigger.as_ref().map_or_else(
                || "unknown".into(),
                |kind| serde_json::to_string(kind).expect("event kind"),
            ),
            item.collection_status
        )
        .unwrap();
        for gap in &item.diagnostics {
            writeln!(text, "  gap: {gap}").unwrap();
        }
    }
    for gap in &report.diagnostics {
        writeln!(text, "gap: {gap}").unwrap();
    }
    if let Some(evidence) = &report.incident {
        writeln!(
            text,
            "Declared impact: {}\nRecorded evidence (paths may be sensitive):",
            evidence.declared_impact
        )
        .unwrap();
        if let Some(graph) = evidence
            .resources
            .as_ref()
            .and_then(|r| r.relationships.as_ref())
        {
            let trigger = evidence
                .beginning
                .as_ref()
                .and_then(|b| b.capture.trigger.service.as_deref())
                .unwrap_or("");
            text.push_str(
                &graph.render(trigger, &evidence.resources.as_ref().unwrap().observations),
            );
        }
        // JSON escaping keeps untrusted paths/control bytes safe in terminals.
        text.push_str(&serde_json::to_string_pretty(evidence).expect("typed evidence"));
    }
    text
}

pub(in crate::cli) async fn serve(
    stream: &mut crate::cli::transport::Stream,
    reader: Option<Reader>,
    id: Option<String>,
    identity: instances::Identity,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    mut stopping: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let Some(permit) = permit else {
        return protocol::write(
            stream,
            &Response::Error("too many incident readers (maximum 2)".into()),
        )
        .await;
    };
    let Some(reader) = reader else {
        return protocol::write(stream, &Response::Error("incident recording is disabled for this run; use --stored after shutdown for previous runs".into())).await;
    };
    if *stopping.borrow() {
        return protocol::write(stream, &Response::Error("supervisor is stopping".into())).await;
    }
    let control = Control::new();
    let _cancel = CancelOnDrop(control.clone());
    let work = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        reader.read(id, identity.instance_id, Some(identity.run_id), &control)
    });
    use tokio::io::AsyncReadExt;
    let mut byte = [0u8; 1];
    let result = tokio::select! {
        biased;
        _ = stream.read(&mut byte) => return Ok(()),
        _ = stopping.changed() => return Ok(()),
        _ = tokio::time::sleep(DEADLINE) => Response::Error("incident query exceeded 30s".into()),
        result = work => match result.context("incident reader failed").and_then(|r| r) {
            Ok(report) => Response::Incidents(Box::new(report)),
            Err(error) => Response::Error(format!("cannot read incidents: {error:#}")),
        }
    };
    protocol::write(stream, &result).await
}

#[cfg(test)]
mod tests;
