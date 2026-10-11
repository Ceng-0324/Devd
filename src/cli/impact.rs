//! Read-only impact view over declared paths and service dependency edges.
use std::{
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use serde::Serialize;
use tokio::io::AsyncWriteExt;

use crate::{
    config::{ConfigLoader, DevdConfig},
    core::resource_impact::{self, Field, Selector, Target, View},
};

use super::instances;

const MAX_PEERS: usize = 8;
const MAX_PEER_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Scope {
    #[default]
    Project,
    Worktrees,
}

#[derive(clap::Args)]
pub(super) struct Args {
    /// Service declaring the resource.
    service: String,
    /// Zero-based index in this service's effective requires list.
    #[arg(long, required_unless_present = "path", conflicts_with = "path")]
    require: Option<usize>,
    /// Name in this service's effective paths map.
    #[arg(long, required_unless_present = "require")]
    path: Option<String>,
    /// Include registered instances in the current Git repository and worktrees.
    #[arg(long, value_enum, default_value_t = Scope::Project)]
    scope: Scope,
    /// Print one versioned JSON report.
    #[arg(long)]
    json: bool,
}

#[derive(Serialize)]
struct Report {
    schema_version: u16,
    captured_from: chrono::DateTime<chrono::Utc>,
    captured_to: chrono::DateTime<chrono::Utc>,
    atomic: bool,
    instance_id: String,
    runtime_equivalence: &'static str,
    scope: Scope,
    source: &'static str,
    config: PathBuf,
    profile: Option<String>,
    state_dir: PathBuf,
    selected: Target,
    current: View,
    discovery: Option<Discovery>,
    limits: Limits,
    interpretation: &'static str,
}

#[derive(Serialize)]
struct Discovery {
    project_root: PathBuf,
    registry_complete: bool,
    impact_complete: bool,
    warnings: Vec<String>,
    omitted_peers: usize,
    peers: Vec<Peer>,
}

#[derive(Serialize)]
struct Peer {
    instance_id: String,
    run_id: String,
    project_root: PathBuf,
    config: PathBuf,
    profile: Option<String>,
    state_dir: PathBuf,
    registry_status: &'static str,
    config_source: &'static str,
    runtime_equivalence: &'static str,
    view: Option<View>,
    warning: Option<String>,
}

#[derive(Serialize)]
struct Limits {
    services_per_config: usize,
    dependency_edges_per_config: usize,
    declarations_per_config: usize,
    matches_per_config: usize,
    peers: usize,
    peer_config_bytes: u64,
    output_bytes: usize,
}

pub(super) async fn run(
    args: Args,
    config_path: &Path,
    profile: Option<&str>,
    state_dir: &Path,
) -> Result<()> {
    let captured_from = chrono::Utc::now();
    let normalized_state = tokio::fs::canonicalize(state_dir)
        .await
        .unwrap_or_else(|_| state_dir.to_owned());
    let instance_id = instances::instance_id(config_path, &normalized_state, profile)?;
    let config = super::load_config(config_path, profile).await?;
    let field = match (args.require, args.path) {
        (Some(index), None) => Field::Requires { index },
        (None, Some(name)) => Field::Paths { name },
        _ => bail!("choose exactly one of --require or --path"),
    };
    let selector = Selector {
        service: args.service,
        field,
    };
    let state = state_dir.to_owned();
    let source = config;
    let (selected, current) = tokio::task::spawn_blocking(move || -> Result<_> {
        let selected = resource_impact::select(&source, &state, selector)?;
        let current = resource_impact::analyze(&source, &state, &selected)?;
        if !current
            .matches
            .iter()
            .any(|entry| entry.selector == selected.selector)
        {
            bail!("selected declaration lies beyond the impact scan limit");
        }
        Ok((selected, current))
    })
    .await
    .context("impact worker failed")??;

    let discovery = match args.scope {
        Scope::Project => None,
        Scope::Worktrees => Some(discover_peers(config_path, &instance_id, &selected).await?),
    };
    let interpretation = "Declared potential reachability only; not observed application file access, failure causation, live configuration equivalence, or deletion authorization.";
    let report = Report {
        schema_version: 1,
        captured_from,
        captured_to: chrono::Utc::now(),
        atomic: false,
        instance_id,
        runtime_equivalence: "unknown",
        scope: args.scope,
        source: "current-disk-effective-config",
        config: config_path.to_owned(),
        profile: profile.map(str::to_owned),
        state_dir: state_dir.to_owned(),
        selected,
        current,
        discovery,
        limits: Limits {
            services_per_config: resource_impact::MAX_SERVICES,
            dependency_edges_per_config: resource_impact::MAX_EDGES,
            declarations_per_config: resource_impact::MAX_DECLARATIONS,
            matches_per_config: resource_impact::MAX_MATCHES,
            peers: MAX_PEERS,
            peer_config_bytes: MAX_PEER_CONFIG_BYTES,
            output_bytes: MAX_OUTPUT_BYTES,
        },
        interpretation,
    };
    let output = if args.json {
        format!("{}\n", serde_json::to_string_pretty(&report)?)
    } else {
        render(&report)
    };
    if output.len() > MAX_OUTPUT_BYTES {
        bail!("impact output exceeds the {MAX_OUTPUT_BYTES}-byte limit; narrow the query scope");
    }
    write_output(&output).await
}

async fn discover_peers(
    config_path: &Path,
    current_id: &str,
    target: &Target,
) -> Result<Discovery> {
    let report = instances::discover(
        config_path
            .parent()
            .context("missing configuration directory")?,
    )
    .await?;
    let mut peers = Vec::new();
    let mut omitted_peers = 0;
    for entry in report.entries {
        if entry.identity.instance_id == current_id {
            continue;
        }
        if peers.len() == MAX_PEERS {
            omitted_peers += 1;
            continue;
        }
        let identity = entry.identity;
        let mut peer = Peer {
            instance_id: identity.instance_id,
            run_id: identity.run_id,
            project_root: identity.project_root,
            config: identity.config,
            profile: identity.profile,
            state_dir: identity.state_dir,
            registry_status: entry.status,
            config_source: "not-read",
            runtime_equivalence: "unknown",
            view: None,
            warning: None,
        };
        if entry.status == "live" {
            match read_peer_config(&peer.config, peer.profile.as_deref()).await {
                Ok(config) => {
                    let state = peer.state_dir.clone();
                    let target_key = target.clone();
                    // Only immutable selector/path data enters the blocking task.
                    match tokio::task::spawn_blocking(move || {
                        resource_impact::analyze(&config, &state, &target_key)
                    })
                    .await
                    .context("peer impact worker failed")?
                    {
                        Ok(view) => {
                            peer.config_source = "disk-candidate";
                            peer.view = Some(view);
                        }
                        Err(error) => {
                            peer.warning =
                                Some(format!("cannot analyze peer disk configuration: {error}"))
                        }
                    }
                }
                Err(error) => {
                    peer.warning = Some(format!(
                        "cannot read valid peer disk configuration: {error}"
                    ))
                }
            }
        } else {
            peer.warning = Some("registry identity is not live-verified; impact is unknown".into());
        }
        peers.push(peer);
    }
    // Even live peers provide only disk candidates: reload/edit may differ from
    // supervisor memory. Registry coverage is separately reported.
    let impact_complete = report.complete && omitted_peers == 0 && peers.is_empty();
    Ok(Discovery {
        project_root: report.project_root,
        registry_complete: report.complete,
        impact_complete,
        warnings: report.warnings,
        omitted_peers,
        peers,
    })
}

async fn read_peer_config(path: &Path, profile: Option<&str>) -> Result<DevdConfig> {
    let path = path.to_owned();
    let source = path.clone();
    let contents = tokio::task::spawn_blocking(move || -> Result<String> {
        let file = crate::platform::files::open_regular(&source, false, true)?;
        let mut bytes = Vec::new();
        file.take(MAX_PEER_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_PEER_CONFIG_BYTES {
            bail!("peer configuration exceeds {MAX_PEER_CONFIG_BYTES} bytes");
        }
        Ok(String::from_utf8(bytes)?)
    })
    .await
    .context("peer config read worker failed")??;
    let mut config = ConfigLoader::from_str_profile(&contents, &path, profile)
        .map_err(|_| anyhow::anyhow!("peer configuration or selected profile is invalid"))?;
    config
        .validate()
        .map_err(|_| anyhow::anyhow!("peer configuration validation failed"))?;
    super::resolve_working_directories(&mut config, &path);
    Ok(config)
}

fn render(report: &Report) -> String {
    use std::fmt::Write;
    let mut text = format!(
        "Resource impact (declared potential only; {:?}):\n",
        report.scope
    );
    writeln!(
        text,
        "  instance: {} source={} runtime_equivalence={} captured={}..{} atomic=false",
        report.instance_id,
        report.source,
        report.runtime_equivalence,
        report.captured_from,
        report.captured_to
    )
    .unwrap();
    let selected = &report.selected;
    writeln!(
        text,
        "  configuration: {:?} profile={:?} state={:?}",
        report.config, report.profile, report.state_dir
    )
    .unwrap();
    writeln!(
        text,
        "  selected: {:?} {:?} requested={:?} scope={:?}",
        selected.selector.service,
        selected.selector.field,
        selected.requested_path,
        selected.declared_scope
    )
    .unwrap();
    writeln!(
        text,
        "  observed: resolved={:?} failure={:?} failed_node={:?} complete={} atomic=false",
        selected.inspection.resolved,
        selected.inspection.failure,
        selected.inspection.failed_node,
        selected.inspection.complete
    )
    .unwrap();
    for (index, node) in selected.inspection.nodes.iter().enumerate() {
        writeln!(
            text,
            "    node {index}: {:?} {:?} -> {:?}",
            node.kind, node.path, node.target
        )
        .unwrap();
    }
    for hint in &selected.risk_hints {
        writeln!(text, "  risk hint: {hint}").unwrap();
    }
    render_view(&mut text, "current disk", &report.current);
    if let Some(discovery) = &report.discovery {
        writeln!(
            text,
            "  discovery: registry_complete={} impact_complete={} omitted_peers={} project={:?}",
            discovery.registry_complete,
            discovery.impact_complete,
            discovery.omitted_peers,
            discovery.project_root
        )
        .unwrap();
        for warning in &discovery.warnings {
            writeln!(text, "  discovery warning: {warning:?}").unwrap();
        }
        for peer in &discovery.peers {
            writeln!(text, "  peer {} run={} registry={} source={} runtime_equivalence={} config={:?} profile={:?}",
                peer.instance_id, peer.run_id, peer.registry_status, peer.config_source,
                peer.runtime_equivalence, peer.config, peer.profile).unwrap();
            if let Some(view) = &peer.view {
                render_view(&mut text, "peer disk candidate", view);
            }
            if let Some(warning) = &peer.warning {
                writeln!(text, "    unknown: {warning:?}").unwrap();
            }
        }
    }
    writeln!(text, "{}", report.interpretation).unwrap();
    text
}

fn render_view(text: &mut String, label: &str, view: &View) {
    use std::fmt::Write;
    writeln!(
        text,
        "  {label}: complete={} omitted_declarations={} omitted_matches={} uncertain_aliases={}",
        view.complete, view.omitted_declarations, view.omitted_matches, view.uncertain_aliases
    )
    .unwrap();
    for entry in &view.matches {
        writeln!(
            text,
            "    match: {:?} {:?} basis={:?} scope={:?} requested={:?} observed={:?} failure={:?} complete={} conflicts_with_selection={}",
            entry.selector.service,
            entry.selector.field,
            entry.basis,
            entry.declared_scope,
            entry.requested_path, entry.observed_target, entry.observed_failure,
            entry.inspection_complete, entry.conflicts_with_selection
        )
        .unwrap();
    }
    for consumer in &view.direct_consumers {
        writeln!(
            text,
            "    direct: {:?} via {:?}",
            consumer.service,
            consumer
                .reasons
                .iter()
                .map(|reason| (
                    &reason.declaration.service,
                    &reason.declaration.field,
                    reason.relation
                ))
                .collect::<Vec<_>>()
        )
        .unwrap();
    }
    for downstream in &view.downstream {
        writeln!(
            text,
            "    downstream: {:?} via {:?}",
            downstream.service, downstream.via_services
        )
        .unwrap();
    }
}

async fn write_output(text: &str) -> Result<()> {
    let mut signals = crate::platform::shutdown::Shutdown::new(false)?;
    let mut writer = super::stdout::Stdout::new()?;
    write_report(&mut writer, text, signals.recv()).await
}

async fn write_report(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    text: &str,
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<()> {
    tokio::select! {
        biased;
        _ = cancelled => bail!("impact output cancelled; report may be incomplete"),
        result = tokio::time::timeout(super::protocol::IO_TIMEOUT, async {
            writer.write_all(text.as_bytes()).await?;
            writer.flush().await
        }) => result.context("impact output timed out after 5s; report may be incomplete")?
            .context("cannot write impact report"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn test_impact_output_backpressure_times_out() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        let error = write_report(&mut writer, "report", std::future::pending())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out after 5s"));
    }

    #[tokio::test]
    async fn test_impact_output_cancellation_does_not_wait_for_stdout() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        let error = write_report(&mut writer, "report", std::future::ready(()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }

    #[tokio::test]
    async fn test_peer_config_rejects_oversized_invalid_and_special_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("devd.yml");
        std::fs::write(&path, vec![b' '; MAX_PEER_CONFIG_BYTES as usize + 1]).unwrap();
        assert!(read_peer_config(&path, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("exceeds"));
        std::fs::write(&path, "services: secret-command-value").unwrap();
        let error = read_peer_config(&path, None).await.unwrap_err().to_string();
        assert!(error.contains("invalid"));
        assert!(!error.contains("secret-command-value"));
        assert!(read_peer_config(root.path(), None).await.is_err());
        #[cfg(unix)]
        {
            let fifo = root.path().join("fifo");
            nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRUSR).unwrap();
            assert!(read_peer_config(&fifo, None).await.is_err());
        }
    }
}
