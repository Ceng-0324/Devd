//! Read-only explanation of declared diagnostic coverage and known evidence gaps.
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::{
    config::{ConfigLoader, DevdConfig, PathRequirementType, PathScope, ServiceKind},
    core::events::{query::PersistenceState, EVENT_CAPACITY},
    core::service_manager::{RuntimeSnapshot, ServiceState},
    storage::StorageOptions,
};

use super::export::output::{self, CancelOnDrop, Control, DEADLINE};
use super::protocol::{self, Request, Response};

const MAX_SERVICES: usize = 256;
const MAX_RESOURCES: usize = 512;
pub(super) const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

#[derive(clap::Args)]
pub(super) struct Args {
    /// Inspect the effective configuration and recorder state of the live supervisor.
    #[arg(long)]
    online: bool,
    /// Print one versioned JSON report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Report {
    schema_version: u16,
    source: String,
    captured_at: DateTime<Utc>,
    atomic: bool,
    config: PathBuf,
    profile: Option<String>,
    instance_id: Option<String>,
    run_id: Option<String>,
    services: Vec<Service>,
    omitted_services: usize,
    omitted_resources: usize,
    evidence: Evidence,
    findings: Vec<Finding>,
    limitations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Service {
    name: String,
    kind: ServiceKind,
    runtime_status: Option<ServiceState>,
    required_paths: usize,
    monitor_requires_configured: bool,
    requirements: Vec<Requirement>,
    paths: Vec<NamedPath>,
    named_paths: usize,
    cwd_resolved: bool,
    env_file_declared: bool,
    healthcheck_declared: bool,
    resource_limits_declared: bool,
    listen_addresses: usize,
    named_ports: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Requirement {
    index: usize,
    kind: PathRequirementType,
    runtime_monitor_configured: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NamedPath {
    name: String,
    scope: PathScope,
    runtime_monitor_configured: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Evidence {
    event_memory_capacity: usize,
    event_memory_first_available: Option<u64>,
    event_memory_next_sequence: Option<u64>,
    truncated_events_in_memory: Option<usize>,
    events: Channel,
    logs: Channel,
    incidents: Incident,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Channel {
    state: State,
    source: String,
    retention: Option<Retention>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Retention {
    max_file_bytes: u64,
    archives: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Incident {
    state: State,
    source: String,
    max_artifacts: usize,
    max_artifact_bytes: usize,
    dropped_triggers: Option<u64>,
    skipped_events: Option<u64>,
    retention_evicted_this_recorder: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Finding {
    code: String,
    source: String,
    service: Option<String>,
    consequence: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum State {
    Unknown,
    Disabled,
    Recording,
    Failed,
}

impl From<PersistenceState> for State {
    fn from(value: PersistenceState) -> Self {
        match value {
            PersistenceState::Disabled => Self::Disabled,
            PersistenceState::Recording => Self::Recording,
            PersistenceState::Failed => Self::Failed,
        }
    }
}

/// Captured only by the live supervisor. None of these facts are inferred from
/// current disk YAML or from files left by an earlier run.
pub(super) struct Runtime<'a> {
    pub instance_id: &'a str,
    pub run_id: &'a str,
    pub event_state: PersistenceState,
    pub log_state: PersistenceState,
    pub incident_state: PersistenceState,
    pub event_storage: Option<StorageOptions>,
    pub log_storage: Option<StorageOptions>,
    pub event_first_available: u64,
    pub event_next_sequence: u64,
    pub truncated_events: usize,
    pub incident_dropped: u64,
    pub incident_skipped: u64,
    pub incident_evicted: u64,
    pub snapshot: &'a RuntimeSnapshot,
}

fn finding(code: &str, source: &str, service: Option<&str>, consequence: &str) -> Finding {
    Finding {
        code: code.into(),
        source: source.into(),
        service: service.map(str::to_owned),
        consequence: consequence.into(),
    }
}

fn channel(state: State, source: &str, options: Option<StorageOptions>) -> Channel {
    Channel {
        state,
        source: source.into(),
        retention: options.map(|options| Retention {
            max_file_bytes: options.max_file_bytes,
            archives: options.keep,
        }),
    }
}

pub(super) fn collect(
    config: &DevdConfig,
    config_path: &Path,
    profile: Option<&str>,
    runtime: Option<Runtime<'_>>,
) -> Result<Report> {
    let mut findings = vec![finding(
        "undeclared-dependencies-unknown",
        "configuration-scope",
        None,
        "Only declared resources are covered; application file access and implicit dependencies are not discovered.",
    )];
    let mut names: Vec<_> = config.services.keys().collect();
    names.sort();
    let omitted_services = names.len().saturating_sub(MAX_SERVICES);
    let mut remaining = MAX_RESOURCES;
    let total_resources: usize = config
        .services
        .values()
        .map(|s| s.requires.len() + s.paths.len())
        .sum();
    let mut services = Vec::with_capacity(names.len().min(MAX_SERVICES));
    for name in names.into_iter().take(MAX_SERVICES) {
        let definition = &config.services[name];
        let mut requirements = Vec::new();
        for (index, requirement) in definition.requires.iter().enumerate() {
            if remaining == 0 {
                continue;
            }
            remaining -= 1;
            requirements.push(Requirement {
                index,
                kind: requirement.kind,
                runtime_monitor_configured: definition.monitor_requires,
            });
        }
        let mut paths = Vec::new();
        for (path, binding) in &definition.paths {
            if remaining == 0 {
                continue;
            }
            remaining -= 1;
            paths.push(NamedPath {
                name: path.clone(),
                scope: binding.scope,
                runtime_monitor_configured: false,
            });
        }
        if !definition.requires.is_empty() && !definition.monitor_requires {
            findings.push(finding(
                "requires-not-monitored",
                "effective-configuration",
                Some(name),
                "Required paths are checked before spawn, but changes during execution will not produce path-condition events.",
            ));
        }
        if !definition.paths.is_empty() {
            findings.push(finding(
                "path-mappings-not-monitored",
                "effective-configuration",
                Some(name),
                "Named path mappings are passed to services, not monitored for changes by this declaration.",
            ));
        }
        services.push(Service {
            name: name.clone(),
            kind: definition.kind,
            runtime_status: runtime
                .as_ref()
                .and_then(|r| r.snapshot.services.get(name))
                .map(|s| s.status),
            required_paths: definition.requires.len(),
            monitor_requires_configured: definition.monitor_requires,
            requirements,
            paths,
            named_paths: definition.paths.len(),
            cwd_resolved: definition.cwd.is_some(),
            env_file_declared: definition.env_file.is_some(),
            healthcheck_declared: definition.healthcheck.is_some(),
            resource_limits_declared: definition.limits.is_some(),
            listen_addresses: definition.listen.len(),
            named_ports: definition.ports.len(),
        });
    }
    let omitted_resources = total_resources - (MAX_RESOURCES - remaining);
    if omitted_services > 0 || omitted_resources > 0 {
        findings.push(finding(
            "coverage-output-truncated",
            "report-capacity",
            None,
            "Some declared services or resources were omitted; this report is not a complete inventory.",
        ));
    }
    let (source, instance_id, run_id, evidence) = if let Some(runtime) = runtime {
        if runtime.event_first_available > 0 {
            findings.push(finding(
                "live-event-window-evicted",
                "live-event-buffer",
                None,
                "Earlier events are absent from live memory; persisted history may retain some of them, but was not inspected here.",
            ));
        }
        if runtime.truncated_events > 0 {
            findings.push(finding("retained-events-truncated", "live-event-buffer", None,
                "Some retained event payloads were truncated; missing details cannot be reconstructed from this window."));
        }
        if runtime.incident_dropped > 0 || runtime.incident_skipped > 0 {
            findings.push(finding(
                "incident-recorder-gap",
                "live-incident-recorder",
                None,
                "Some eligible triggers or context events were missed by the incident recorder in this run.",
            ));
        }
        if runtime.incident_evicted > 0 {
            findings.push(finding(
                "incident-retention-evicted",
                "live-incident-storage",
                None,
                "This recorder evicted older artifacts, possibly from earlier runs; absence no longer proves no incident occurred.",
            ));
        }
        for (name, state) in [
            ("events", runtime.event_state),
            ("logs", runtime.log_state),
            ("incidents", runtime.incident_state),
        ] {
            if state != PersistenceState::Recording {
                findings.push(finding(
                    &format!(
                        "{name}-{}",
                        if state == PersistenceState::Failed {
                            "failed"
                        } else {
                            "disabled"
                        }
                    ),
                    "live-supervisor",
                    None,
                    if state == PersistenceState::Failed {
                        "This run's recording failed; retained evidence may be incomplete."
                    } else {
                        "This run does not record this evidence to disk."
                    },
                ));
            }
        }
        (
            "live-supervisor".into(),
            Some(runtime.instance_id.into()),
            Some(runtime.run_id.into()),
            Evidence {
                event_memory_capacity: EVENT_CAPACITY,
                event_memory_first_available: Some(runtime.event_first_available),
                event_memory_next_sequence: Some(runtime.event_next_sequence),
                truncated_events_in_memory: Some(runtime.truncated_events),
                events: channel(
                    runtime.event_state.into(),
                    "live-supervisor",
                    runtime.event_storage,
                ),
                logs: channel(
                    runtime.log_state.into(),
                    "live-supervisor",
                    runtime.log_storage,
                ),
                incidents: Incident {
                    state: runtime.incident_state.into(),
                    source: "live-supervisor".into(),
                    max_artifacts: super::incidents::storage::KEEP,
                    max_artifact_bytes: super::incidents::storage::MAX_BYTES,
                    dropped_triggers: Some(runtime.incident_dropped),
                    skipped_events: Some(runtime.incident_skipped),
                    retention_evicted_this_recorder: Some(runtime.incident_evicted),
                },
            },
        )
    } else {
        findings.push(finding(
            "runtime-recording-unknown",
            "effective-configuration",
            None,
            "Recording is selected by start flags, not YAML; this static report cannot establish whether a supervisor is running or retaining evidence.",
        ));
        (
            "effective-configuration".into(),
            None,
            None,
            Evidence {
                event_memory_capacity: EVENT_CAPACITY,
                event_memory_first_available: None,
                event_memory_next_sequence: None,
                truncated_events_in_memory: None,
                events: channel(State::Unknown, "not-inspected-static", None),
                logs: channel(State::Unknown, "not-inspected-static", None),
                incidents: Incident {
                    state: State::Unknown,
                    source: "not-inspected-static".into(),
                    max_artifacts: super::incidents::storage::KEEP,
                    max_artifact_bytes: super::incidents::storage::MAX_BYTES,
                    dropped_triggers: None,
                    skipped_events: None,
                    retention_evicted_this_recorder: None,
                },
            },
        )
    };
    let report = Report {
        schema_version: 1,
        source,
        captured_at: Utc::now(),
        atomic: false,
        config: config_path.to_owned(),
        profile: profile.map(str::to_owned),
        instance_id,
        run_id,
        services,
        omitted_services,
        omitted_resources,
        evidence,
        findings,
        limitations: [
            "Coverage describes declarations and recorder observations, not current health, a reliability score, or proof of file access.",
            "Configured monitors run only during managed process generations; completed tasks no longer monitor. A declaration is not proof that a sample was obtained.",
            "Path polling reports condition changes, can miss transients, and does not detect content or link replacement when the condition stays valid.",
            "cwd, program, env-file, named paths and listening declarations do not independently enable continuous file or port monitoring; health probes do not prove resource ownership.",
            "Resource samples cover the leader PID only and can be unavailable; limits produce threshold events, not a persisted metrics time series.",
            "Disk capacity is a rotation constraint, not a time window or a guarantee that future evidence survives. Recording events, logs and incidents are independent startup choices.",
            "Disk files are not scanned here: storage lag, unfinished records, individual incident omissions and historical losses can remain unknown. Inspect retained events and incidents separately.",
            "Reports omit resource path values, commands, environment contents and application logs; service names, binding names and config paths can still be sensitive.",
        ].into_iter().map(str::to_owned).collect(),
    };
    encode(&report)?;
    Ok(report)
}

fn encode(report: &Report) -> Result<Vec<u8>> {
    let bytes = output::encode(report)?;
    if bytes.len() >= MAX_OUTPUT_BYTES {
        bail!("coverage output exceeds 2 MiB; reduce declared resources");
    }
    Ok(bytes)
}

pub(super) async fn run(
    args: Args,
    config_path: &Path,
    profile: Option<&str>,
    socket: &Path,
) -> Result<()> {
    let mut signals = crate::platform::shutdown::Shutdown::new(false)?;
    let control = Control::new();
    let _cancel = CancelOnDrop(control.clone());
    let work = async {
        if args.online {
            let Response::Coverage(report) = protocol::request(socket, Request::Coverage)
                .await
                .context("live coverage unavailable; no static fallback was used")?
            else {
                bail!("unexpected coverage response");
            };
            if report.schema_version != 1
                || report.source != "live-supervisor"
                || report.config != config_path
                || report.profile.as_deref() != profile
                || report.instance_id.is_none()
                || report.run_id.is_none()
            {
                bail!("coverage response does not match the selected configuration/profile or supported schema");
            }
            Ok(*report)
        } else {
            let path = config_path.to_owned();
            let profile = profile.map(str::to_owned);
            tokio::task::spawn_blocking(move || {
                let bytes = output::read_regular(&path, &control, 1024 * 1024)?.bytes;
                let yaml = std::str::from_utf8(&bytes).context("configuration is not UTF-8")?;
                let mut config = ConfigLoader::from_str_profile(yaml, &path, profile.as_deref())?;
                config.validate()?;
                super::resolve_working_directories(&mut config, &path);
                control.check()?;
                collect(&config, &path, profile.as_deref(), None)
            })
            .await
            .context("coverage configuration reader failed")?
        }
    };
    let report = tokio::select! {
        biased;
        _ = signals.recv() => bail!("coverage cancelled"),
        result = tokio::time::timeout(DEADLINE, work) => result.context("coverage exceeded 30s")??,
    };
    let body = if args.json {
        let mut bytes = encode(&report)?;
        bytes.push(b'\n');
        bytes
    } else {
        render(&report).into_bytes()
    };
    if body.len() > MAX_OUTPUT_BYTES {
        bail!("coverage output exceeds 2 MiB; reduce declared resources");
    }
    let mut stdout = super::stdout::Stdout::new()?;
    write_output(&mut stdout, &body, signals.recv()).await
}

async fn write_output(
    stdout: &mut (impl tokio::io::AsyncWrite + Unpin),
    body: &[u8],
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<()> {
    tokio::select! {
        biased;
        _ = cancelled => bail!("coverage output cancelled; report may be incomplete"),
        result = tokio::time::timeout(protocol::IO_TIMEOUT, async {
            stdout.write_all(body).await?;
            stdout.flush().await
        }) => result.map_err(|_| anyhow::anyhow!("coverage output timed out; report may be incomplete"))??,
    }
    Ok(())
}

fn observed(value: Option<impl std::fmt::Display>) -> String {
    value.map_or_else(|| "unknown".into(), |v| v.to_string())
}

fn render(report: &Report) -> String {
    let mut text = format!(
        "Coverage: {} ({})\nServices: {} shown, {} omitted; resources: {} omitted\n",
        report.source.escape_debug(),
        report
            .profile
            .as_deref()
            .unwrap_or("default")
            .escape_debug(),
        report.services.len(),
        report.omitted_services,
        report.omitted_resources,
    );
    for service in &report.services {
        text.push_str(&format!(
            "{}: {:?}, runtime={}, requires={} (monitor configured={}), paths={}, healthcheck={}, limits={}\n",
            service.name.escape_debug(),
            service.kind,
            service.runtime_status.map_or_else(|| "unknown".into(), |s| format!("{s:?}").to_ascii_lowercase()),
            service.required_paths,
            service.monitor_requires_configured,
            service.named_paths,
            service.healthcheck_declared,
            service.resource_limits_declared,
        ));
        for requirement in &service.requirements {
            text.push_str(&format!(
                "  requires:{}: {:?}, monitor configured={}\n",
                requirement.index, requirement.kind, requirement.runtime_monitor_configured
            ));
        }
        for path in &service.paths {
            text.push_str(&format!(
                "  paths:{}: {:?}, mapping alone enables no monitor\n",
                path.name.escape_debug(),
                path.scope
            ));
        }
    }
    text.push_str(&format!(
        "Event memory: capacity={}, first available={}, next sequence={}, truncated={}\n",
        report.evidence.event_memory_capacity,
        observed(report.evidence.event_memory_first_available),
        observed(report.evidence.event_memory_next_sequence),
        observed(report.evidence.truncated_events_in_memory)
    ));
    for (name, channel) in [
        ("events", &report.evidence.events),
        ("logs", &report.evidence.logs),
    ] {
        text.push_str(&format!("{name}: {:?}", channel.state));
        if let Some(retention) = &channel.retention {
            text.push_str(&format!(
                " ({} bytes/file, {} archives)",
                retention.max_file_bytes, retention.archives
            ));
        }
        text.push('\n');
    }
    text.push_str(&format!(
        "incidents: {:?} (at most {} artifacts); dropped={}, skipped={}, evicted={}\n",
        report.evidence.incidents.state,
        report.evidence.incidents.max_artifacts,
        observed(report.evidence.incidents.dropped_triggers),
        observed(report.evidence.incidents.skipped_events),
        observed(report.evidence.incidents.retention_evicted_this_recorder),
    ));
    for finding in &report.findings {
        text.push_str(&format!(
            "{}{} [{}]: {}\n",
            finding
                .service
                .as_ref()
                .map_or(String::new(), |s| format!("{}: ", s.escape_debug())),
            finding.code.escape_debug(),
            finding.source.escape_debug(),
            finding.consequence.escape_debug(),
        ));
    }
    for limitation in &report.limitations {
        text.push_str(&format!("Note: {}\n", limitation.escape_debug()));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(monitored: bool) -> DevdConfig {
        ConfigLoader::from_str(&format!("version: '1'\nservices:\n  build:\n    kind: task\n    command: secret-command\n    env: {{SECRET: secret-value}}\n    requires: [{{type: file, path: secret-path}}]\n    paths: {{CACHE: {{scope: shared, path: secret-mapping}}}}\n    monitor-requires: {monitored}\n"), "devd.yml").unwrap()
    }

    #[test]
    fn test_coverage_static_monitoring_and_recording_are_separate_and_private() {
        for monitored in [false, true] {
            let report = collect(&config(monitored), Path::new("devd.yml"), None, None).unwrap();
            assert_eq!(report.evidence.events.state, State::Unknown);
            assert_eq!(report.evidence.incidents.state, State::Unknown);
            assert!(report.evidence.events.retention.is_none());
            assert!(report.run_id.is_none());
            assert!(report.services[0].runtime_status.is_none());
            assert_eq!(
                report.services[0].requirements[0].runtime_monitor_configured,
                monitored
            );
            assert_eq!(
                report
                    .findings
                    .iter()
                    .any(|f| f.code == "requires-not-monitored"),
                !monitored
            );
            assert!(!String::from_utf8(encode(&report).unwrap())
                .unwrap()
                .contains("secret-"));
        }
    }

    #[test]
    fn test_coverage_live_failure_eviction_and_retention_are_independent() {
        let snapshot = RuntimeSnapshot {
            supervisor_pid: 1,
            event_run_id: Some("run".into()),
            services: Default::default(),
        };
        let report = collect(
            &config(true),
            Path::new("devd.yml"),
            None,
            Some(Runtime {
                instance_id: "instance",
                run_id: "run",
                snapshot: &snapshot,
                event_state: PersistenceState::Failed,
                log_state: PersistenceState::Disabled,
                incident_state: PersistenceState::Recording,
                event_storage: Some(StorageOptions {
                    max_file_bytes: 1024 * 1024,
                    keep: 1,
                }),
                log_storage: None,
                event_first_available: 40,
                event_next_sequence: 1064,
                truncated_events: 2,
                incident_dropped: 3,
                incident_skipped: 7,
                incident_evicted: 9,
            }),
        )
        .unwrap();
        assert_eq!(report.evidence.events.state, State::Failed);
        assert_eq!(report.evidence.logs.state, State::Disabled);
        assert_eq!(report.evidence.incidents.state, State::Recording);
        assert_eq!(report.evidence.events.retention.unwrap().archives, 1);
        for code in [
            "events-failed",
            "logs-disabled",
            "live-event-window-evicted",
            "incident-recorder-gap",
            "incident-retention-evicted",
            "retained-events-truncated",
        ] {
            assert!(report.findings.iter().any(|f| f.code == code), "{code}");
        }
        assert_eq!(
            report.evidence.incidents.retention_evicted_this_recorder,
            Some(9)
        );
    }

    #[test]
    fn test_coverage_inventory_bounds_are_explicit_and_deterministic() {
        let mut config = config(true);
        let service = config.services.remove("build").unwrap();
        for n in 0..300 {
            config
                .services
                .insert(format!("service-{n:03}"), service.clone());
        }
        let report = collect(&config, Path::new("devd.yml"), None, None).unwrap();
        assert_eq!(report.services.len(), MAX_SERVICES);
        assert_eq!(report.omitted_services, 44);
        assert_eq!(report.omitted_resources, 88);
        assert_eq!(report.services.first().unwrap().name, "service-000");
        assert_eq!(report.services.last().unwrap().name, "service-255");
        assert!(report
            .findings
            .iter()
            .any(|f| f.code == "coverage-output-truncated"));
    }

    #[tokio::test(start_paused = true)]
    async fn test_coverage_output_is_bounded_and_cancellable() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        assert!(
            write_output(&mut writer, b"blocked", std::future::pending())
                .await
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        assert!(write_output(&mut writer, b"blocked", async {})
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
    }

    #[test]
    fn test_coverage_byte_limit_and_terminal_fields() {
        let mut config = config(false);
        let service = config.services.remove("build").unwrap();
        config
            .services
            .insert("a".repeat(MAX_OUTPUT_BYTES), service);
        assert!(collect(&config, Path::new("devd.yml"), None, None)
            .unwrap_err()
            .to_string()
            .contains("exceeds 2 MiB"));
        let mut report = collect(&self::config(false), Path::new("devd.yml"), None, None).unwrap();
        report.services[0].name = "line\n\u{1b}[2J".into();
        report.limitations.push("line\n\u{1b}[2J".into());
        let text = render(&report);
        assert!(!text.contains('\u{1b}'));
        assert!(text.contains("line\\n\\u{1b}[2J"));
    }
}
