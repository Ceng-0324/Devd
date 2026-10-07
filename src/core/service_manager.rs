use std::{collections::BTreeMap, future::Future, io, path::PathBuf, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch},
    task::{Id, JoinError, JoinSet},
    time::Instant,
};

use crate::config::{BackoffType, ConfigValidationError, DevdConfig, ResourceThresholds};
use crate::logging::{LogCollector, LogEntry, LogHistory, LogOptions, LogOptionsError};

use super::{
    dependency::DependencyGraph,
    events::{
        EventData, EventHistory, EventRecorder, LifecycleEvent, ResourceEvidence, ShutdownReason,
    },
    health_check::{HealthCheckError, HealthChecker},
    process_manager::{parse_command, ProcessError},
    resource_monitor::{self, ResourceUsage},
    service_task::{Control, ServiceTask},
    state_store::StateStore,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceState {
    Pending,
    Starting,
    Running,
    Healthy,
    Unhealthy,
    Restarting,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceSnapshot {
    pub status: ServiceState,
    pub pid: Option<u32>,
    pub started_at: Option<DateTime<Utc>>,
    pub restart_count: u32,
    pub consecutive_failures: u32,
    pub last_exit_code: Option<i32>,
    pub last_exit_signal: Option<i32>,
    pub last_error: Option<String>,
    /// Latest sample of the service leader; absent before sampling or after exit.
    #[serde(default)]
    pub resources: Option<ResourceUsage>,
    /// Pending automatic restart decision for this live generation only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_restart_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_restart_evidence: Option<ResourceEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_restart_event: Option<u64>,
}

impl Default for ServiceSnapshot {
    fn default() -> Self {
        Self {
            status: ServiceState::Pending,
            pid: None,
            started_at: None,
            restart_count: 0,
            consecutive_failures: 0,
            last_exit_code: None,
            last_exit_signal: None,
            last_error: None,
            resources: None,
            resource_restart_reason: None,
            event_generation: None,
            resource_restart_evidence: None,
            resource_restart_event: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeSnapshot {
    pub supervisor_pid: u32,
    /// None when reading state files produced before lifecycle events existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_run_id: Option<String>,
    pub services: BTreeMap<String, ServiceSnapshot>,
}

#[derive(Debug, Clone)]
pub struct ManagerOptions {
    pub state_path: PathBuf,
    pub grace_period: Duration,
    pub dependency_timeout: Duration,
    pub logging: LogOptions,
    /// Selected configuration profile, for lifecycle provenance.
    pub profile: Option<String>,
}

impl ManagerOptions {
    /// The caller chooses a state file per project; no daemon is created.
    pub fn new(state_path: impl Into<PathBuf>) -> Self {
        Self {
            state_path: state_path.into(),
            grace_period: Duration::from_secs(5),
            dependency_timeout: Duration::from_secs(30),
            logging: LogOptions::default(),
            profile: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum ServiceManagerError {
    #[error(transparent)]
    Command(#[from] ProcessError),
    #[error(transparent)]
    LogOptions(#[from] LogOptionsError),
    #[error(transparent)]
    InvalidConfig(#[from] ConfigValidationError),
    #[error("service '{service}' health-check setup failed: {source}")]
    Health {
        service: String,
        #[source]
        source: HealthCheckError,
    },
    #[error("{field} duration {duration:?} does not fit the runtime timer")]
    InvalidDuration { field: String, duration: Duration },
    #[error("dependency timeout must be positive")]
    ZeroDependencyTimeout,
    #[error("state-file operation failed for {path}: {source}")]
    StateIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to install shutdown signal handler: {0}")]
    Signal(#[source] io::Error),
    #[error("service task failed: {0}")]
    Task(#[source] JoinError),
    #[error("services failed: {failures:?}")]
    FailedServices { failures: BTreeMap<String, String> },
}

/// Owns all service actors. Subscribe before calling run. A terminal failure
/// stops the whole stack; dropping the run future aborts actors and kills groups.
pub struct ServiceManager {
    config: DevdConfig,
    options: ManagerOptions,
    layers: Vec<Vec<String>>,
    checkers: BTreeMap<String, Option<HealthChecker>>,
    snapshots: watch::Sender<RuntimeSnapshot>,
    logs: LogCollector,
    events: EventRecorder,
    resource_limits: BTreeMap<String, ResourceThresholds>,
    commands: mpsc::Receiver<RestartRequest>,
    controller: ServiceController,
}

struct RestartRequest {
    service: String,
    reply: oneshot::Sender<Result<ServiceSnapshot, String>>,
}

/// Requests are serialized by the manager; actors retain process ownership.
#[derive(Clone)]
pub struct ServiceController(mpsc::Sender<RestartRequest>);

impl ServiceController {
    pub async fn restart(&self, service: String) -> Result<ServiceSnapshot, String> {
        let (reply, response) = oneshot::channel();
        self.0
            .send(RestartRequest { service, reply })
            .await
            .map_err(|_| "supervisor is stopping".to_string())?;
        response
            .await
            .map_err(|_| "supervisor stopped before restart completed".to_string())?
    }
}

impl ServiceManager {
    /// Validate every service before any child or state file is created.
    pub fn new(config: DevdConfig, options: ManagerOptions) -> Result<Self, ServiceManagerError> {
        config.validate()?;
        if options.dependency_timeout.is_zero() {
            return Err(ServiceManagerError::ZeroDependencyTimeout);
        }
        for (field, duration) in [
            ("grace-period", options.grace_period),
            ("dependency-timeout", options.dependency_timeout),
        ] {
            validate_duration(field.into(), duration)?;
        }
        let graph = DependencyGraph::from_config(&config).map_err(ConfigValidationError::from)?;
        let layers = graph
            .startup_layers()
            .map_err(ConfigValidationError::from)?;
        let mut checkers = BTreeMap::new();
        for name in graph.service_names() {
            let service = &config.services[name];
            parse_command(name, &service.command)?;
            if service.restart.backoff == BackoffType::Exponential {
                validate_duration(
                    format!("services.{name}.restart.max-delay"),
                    service.restart.max_delay,
                )?;
            }
            validate_duration(
                format!("services.{name}.restart.initial-delay"),
                service.restart.initial_delay,
            )?;
            let checker = service
                .healthcheck
                .as_ref()
                .map(|check| HealthChecker::for_service(check, service))
                .transpose()
                .map_err(|source| ServiceManagerError::Health {
                    service: name.into(),
                    source,
                })?;
            checkers.insert(name.into(), checker);
        }
        let resource_limits = config
            .services
            .iter()
            .filter_map(|(name, service)| {
                service.limits.as_ref().map(|limits| {
                    (
                        name.clone(),
                        limits.thresholds().expect("validated resource limits"),
                    )
                })
            })
            .collect();
        let events = EventRecorder::new(options.state_path.clone(), options.profile.clone());
        let initial = RuntimeSnapshot {
            supervisor_pid: std::process::id(),
            event_run_id: Some(events.run_id().to_owned()),
            services: graph
                .service_names()
                .map(|name| (name.into(), ServiceSnapshot::default()))
                .collect(),
        };
        let (snapshots, _) = watch::channel(initial);
        let logs = LogCollector::new(options.logging)?;
        let (sender, commands) = mpsc::channel(32);
        Ok(Self {
            config,
            options,
            layers,
            checkers,
            snapshots,
            logs,
            events,
            resource_limits,
            commands,
            controller: ServiceController(sender),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<RuntimeSnapshot> {
        self.snapshots.subscribe()
    }

    pub fn subscribe_logs(&self) -> broadcast::Receiver<Arc<LogEntry>> {
        self.logs.subscribe()
    }

    pub fn log_history(&self) -> LogHistory {
        self.logs.history()
    }

    /// Return the bounded lifecycle history for this supervisor run.
    pub fn event_history(&self) -> EventHistory {
        self.events.history()
    }

    /// Subscribe to structured lifecycle events. Subscribers may lag; the
    /// bounded history remains the source for a fresh snapshot.
    pub fn subscribe_events(&self) -> broadcast::Receiver<Arc<LifecycleEvent>> {
        self.events.subscribe()
    }

    pub fn controller(&self) -> ServiceController {
        self.controller.clone()
    }

    /// Install shutdown handlers before spawning children (SIGINT/SIGTERM on
    /// Unix, Ctrl+C/Ctrl+Break on Windows).
    pub async fn run(self) -> Result<RuntimeSnapshot, ServiceManagerError> {
        let mut signals =
            crate::platform::shutdown::Shutdown::new(false).map_err(ServiceManagerError::Signal)?;
        self.run_until(async move { signals.recv().await }).await
    }

    /// Run in the foreground until cancellation, terminal failure, or all service
    /// tasks finish. Shutdown quiesces all actors, then stops reverse graph layers.
    /// State writes use atomic replacement and a nonblocking exclusive file lock.
    pub async fn run_until(
        self,
        shutdown: impl Future<Output = ()>,
    ) -> Result<RuntimeSnapshot, ServiceManagerError> {
        let store = Arc::new(StateStore::open(&self.options.state_path).await?);
        self.run_with_store(shutdown, store).await
    }

    pub(crate) async fn run_with_store(
        mut self,
        shutdown: impl Future<Output = ()>,
        store: Arc<StateStore>,
    ) -> Result<RuntimeSnapshot, ServiceManagerError> {
        let initial = self.snapshots.borrow().clone();
        store.write(&initial).await?;
        self.events
            .record(None, None, None, EventData::SupervisorStarted);
        let mut event_guard = RunEventGuard {
            events: self.events.clone(),
            finished: false,
        };
        tokio::pin!(shutdown);
        let shutdown_requested = tokio::select! {
            biased;
            _ = &mut shutdown => true,
            _ = std::future::ready(()) => false,
        };
        let mut updates = self.snapshots.subscribe();
        let mut tasks = JoinSet::new();
        let mut names = BTreeMap::new();
        let mut controls = BTreeMap::new();
        for name in self.checkers.keys() {
            let (control, receiver) = watch::channel(if shutdown_requested {
                Control::Quiescing
            } else {
                Control::Running
            });
            controls.insert(name.clone(), control);
            let id = self.spawn_actor(name, receiver, &store, &mut tasks, None);
            names.insert(id, name.clone());
        }
        let mut restarting: BTreeMap<String, oneshot::Sender<Result<ServiceSnapshot, String>>> =
            BTreeMap::new();
        let mut starting: BTreeMap<String, oneshot::Sender<Result<ServiceSnapshot, String>>> =
            BTreeMap::new();
        let mut error = None;
        let mut shutdown_reason = if shutdown_requested {
            ShutdownReason::Requested
        } else {
            ShutdownReason::Completed
        };
        let monitor = resource_monitor::run(
            self.snapshots.clone(),
            self.logs.clone(),
            self.resource_limits.clone(),
            self.events.clone(),
        );
        tokio::pin!(monitor);
        while !shutdown_requested && !tasks.is_empty() {
            tokio::select! {
                biased;
                _ = &mut shutdown => { shutdown_reason = ShutdownReason::Requested; break; },
                _ = &mut monitor => {},
                Some(request) = self.commands.recv() => {
                    let name = request.service;
                    let rejection = if !controls.contains_key(&name) {
                        Some(format!("unknown service '{name}'"))
                    } else if restarting.contains_key(&name) || starting.contains_key(&name) {
                        Some(format!("service '{name}' is already restarting"))
                    } else { None };
                    if let Some(error) = rejection {
                        let _ = request.reply.send(Err(error));
                    } else {
                        let cause = self.events.record(
                            Some(&name),
                            self.snapshots.borrow().services[&name].event_generation,
                            None,
                            EventData::ManualRestartRequested,
                        );
                        controls[&name].send_replace(Control::Restarting(cause));
                        restarting.insert(name, request.reply);
                    }
                },
                _ = updates.changed() => {
                    let snapshot = updates.borrow_and_update().clone();
                    let completed: Vec<_> = starting.keys().filter(|name| {
                        matches!(snapshot.services[*name].status, ServiceState::Running | ServiceState::Healthy | ServiceState::Unhealthy | ServiceState::Stopped | ServiceState::Failed)
                    }).cloned().collect();
                    for name in completed {
                        let state = snapshot.services[&name].clone();
                        let result = if state.pid.is_some() { Ok(state) } else {
                            Err(state.last_error.unwrap_or_else(|| format!("service '{name}' exited before restart completed")))
                        };
                        let _ = starting.remove(&name).unwrap().send(result);
                    }
                    if let Err(failure) = store.write(&snapshot).await {
                        error = Some(failure);
                        shutdown_reason = ShutdownReason::StateWriteFailed;
                        break;
                    }
                    if snapshot.services.values().any(|s| s.status == ServiceState::Failed) {
                        shutdown_reason = ShutdownReason::ServiceFailed;
                        break;
                    }
                },
                Some(result) = tasks.join_next_with_id() => {
                    if let Some(failure) = collect_task(result, &mut names, &self.snapshots, &self.events) {
                        error = Some(failure);
                        shutdown_reason = ShutdownReason::ActorFailed;
                        break;
                    }
                },
            }
            let ready: Vec<_> = restarting
                .keys()
                .filter(|name| !names.values().any(|active| active == *name))
                .cloned()
                .collect();
            for name in ready {
                self.snapshots.send_modify(|snapshot| {
                    let state = snapshot.services.get_mut(&name).unwrap();
                    state.status = ServiceState::Restarting;
                    state.restart_count = state.restart_count.saturating_add(1);
                });
                let cause = match *controls[&name].borrow() {
                    Control::Restarting(cause) => Some(cause),
                    _ => None,
                };
                let (control, receiver) = watch::channel(Control::Running);
                controls.insert(name.clone(), control);
                let id = self.spawn_actor(&name, receiver, &store, &mut tasks, cause);
                names.insert(id, name.clone());
                starting.insert(name.clone(), restarting.remove(&name).unwrap());
            }
        }
        if shutdown_reason == ShutdownReason::Completed
            && self
                .snapshots
                .borrow()
                .services
                .values()
                .any(|s| s.status == ServiceState::Failed)
        {
            shutdown_reason = ShutdownReason::ServiceFailed;
        }
        let stop_cause = self.events.record(
            None,
            None,
            None,
            EventData::SupervisorStopping {
                reason: shutdown_reason,
            },
        );
        for control in controls.values() {
            control.send_replace(Control::Quiescing);
        }
        for layer in self.layers.iter().rev() {
            for name in layer {
                controls[name].send_replace(Control::Stopping(Some(stop_cause)));
            }
            while names.values().any(|name| layer.contains(name)) {
                if let Some(result) = tasks.join_next_with_id().await {
                    if let Some(failure) =
                        collect_task(result, &mut names, &self.snapshots, &self.events)
                    {
                        error.get_or_insert(failure);
                    }
                }
            }
            let snapshot = self.snapshots.borrow().clone();
            if let Err(failure) = store.write(&snapshot).await {
                error.get_or_insert(failure);
            }
        }
        let snapshot = self.snapshots.borrow().clone();
        for (name, reply) in restarting.into_iter().chain(starting) {
            let message = snapshot.services[&name]
                .last_error
                .clone()
                .unwrap_or_else(|| "supervisor stopped before restart completed".into());
            let _ = reply.send(Err(message));
        }
        event_guard.finished = true;
        self.events.record(
            None,
            None,
            None,
            EventData::SupervisorStopped {
                failed: error.is_some()
                    || snapshot
                        .services
                        .values()
                        .any(|s| s.status == ServiceState::Failed),
            },
        );
        if let Some(error) = error {
            return Err(error);
        }
        let failures: BTreeMap<_, _> = snapshot
            .services
            .iter()
            .filter(|(_, state)| state.status == ServiceState::Failed)
            .map(|(name, state)| {
                (
                    name.clone(),
                    state
                        .last_error
                        .clone()
                        .unwrap_or_else(|| "service failed".into()),
                )
            })
            .collect();
        if !failures.is_empty() {
            return Err(ServiceManagerError::FailedServices { failures });
        }
        Ok(snapshot)
    }

    fn spawn_actor(
        &self,
        name: &str,
        receiver: watch::Receiver<Control>,
        store: &Arc<StateStore>,
        tasks: &mut JoinSet<()>,
        cause: Option<u64>,
    ) -> Id {
        let actor = ServiceTask::new(
            name.into(),
            self.config.services[name].clone(),
            self.checkers[name].clone(),
            self.options.clone(),
            receiver,
            self.snapshots.clone(),
            self.logs.clone(),
            self.events.clone(),
            cause,
        );
        let lease = store.clone();
        tasks
            .spawn(async move {
                let _lease = lease;
                actor.run().await
            })
            .id()
    }
}

struct RunEventGuard {
    events: EventRecorder,
    finished: bool,
}
impl Drop for RunEventGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.events
                .record(None, None, None, EventData::SupervisorCancelled);
        }
    }
}

fn validate_duration(field: String, duration: Duration) -> Result<(), ServiceManagerError> {
    if Instant::now().checked_add(duration).is_none() {
        Err(ServiceManagerError::InvalidDuration { field, duration })
    } else {
        Ok(())
    }
}

fn collect_task(
    result: Result<(Id, ()), JoinError>,
    names: &mut BTreeMap<Id, String>,
    snapshots: &watch::Sender<RuntimeSnapshot>,
    events: &EventRecorder,
) -> Option<ServiceManagerError> {
    match result {
        Ok((id, _)) => {
            names.remove(&id);
            None
        }
        Err(error) => {
            if let Some(name) = names.remove(&error.id()) {
                snapshots.send_modify(|snapshot| {
                    let service = snapshot.services.get_mut(&name).unwrap();
                    service.status = ServiceState::Failed;
                    service.pid = None;
                    service.resources = None;
                    service.resource_restart_reason = None;
                    service.resource_restart_evidence = None;
                    service.resource_restart_event = None;
                    events.record(
                        Some(&name),
                        service.event_generation,
                        None,
                        EventData::ActorFailed {
                            cancelled: error.is_cancelled(),
                        },
                    );
                    service.last_error = Some(error.to_string());
                });
            }
            Some(ServiceManagerError::Task(error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_legacy_snapshot_without_event_fields_remains_readable() {
        let snapshot: RuntimeSnapshot = serde_json::from_value(serde_json::json!({
            "supervisor_pid": 1,
            "services": {
                "worker": {
                    "status": "running", "pid": 123, "started_at": null,
                    "restart_count": 0, "consecutive_failures": 0,
                    "last_exit_code": null, "last_exit_signal": null, "last_error": null
                }
            }
        }))
        .unwrap();
        assert!(snapshot.event_run_id.is_none());
        assert!(snapshot.services["worker"].event_generation.is_none());
        assert!(snapshot.services["worker"]
            .resource_restart_evidence
            .is_none());
        assert!(snapshot.services["worker"].resource_restart_event.is_none());
    }

    #[tokio::test]
    async fn test_failed_actor_clears_pending_resource_restart() {
        let (snapshots, _) = watch::channel(RuntimeSnapshot {
            supervisor_pid: std::process::id(),
            event_run_id: None,
            services: [(
                "worker".into(),
                ServiceSnapshot {
                    pid: Some(123),
                    resource_restart_reason: Some("RSS limit exceeded".into()),
                    ..Default::default()
                },
            )]
            .into(),
        });
        let mut tasks = JoinSet::new();
        let task = tasks.spawn(std::future::pending::<()>());
        let mut names = [(task.id(), "worker".into())].into();
        task.abort();
        assert!(collect_task(
            tasks.join_next_with_id().await.unwrap(),
            &mut names,
            &snapshots,
            &EventRecorder::new("unused".into(), None)
        )
        .is_some());
        let snapshot = snapshots.borrow();
        let state = &snapshot.services["worker"];
        assert_eq!(state.status, ServiceState::Failed);
        assert!(state.pid.is_none() && state.resource_restart_reason.is_none());
    }
}
