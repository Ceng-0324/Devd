use std::{collections::BTreeMap, future::Future, io, path::PathBuf, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    signal::unix::{signal, SignalKind},
    sync::{broadcast, watch},
    task::{Id, JoinError, JoinSet},
    time::Instant,
};

use crate::config::{BackoffType, ConfigValidationError, DevdConfig};
use crate::logging::{LogCollector, LogEntry, LogHistory, LogOptions, LogOptionsError};

use super::{
    dependency::DependencyGraph,
    health_check::{HealthCheckError, HealthChecker},
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceSnapshot {
    pub status: ServiceState,
    pub pid: Option<u32>,
    pub started_at: Option<DateTime<Utc>>,
    pub restart_count: u32,
    pub consecutive_failures: u32,
    pub last_exit_code: Option<i32>,
    pub last_exit_signal: Option<i32>,
    pub last_error: Option<String>,
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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    pub supervisor_pid: u32,
    pub services: BTreeMap<String, ServiceSnapshot>,
}

#[derive(Debug, Clone)]
pub struct ManagerOptions {
    pub state_path: PathBuf,
    pub grace_period: Duration,
    pub dependency_timeout: Duration,
    pub logging: LogOptions,
}

impl ManagerOptions {
    /// The caller chooses a state file per project; no daemon is created.
    pub fn new(state_path: impl Into<PathBuf>) -> Self {
        Self {
            state_path: state_path.into(),
            grace_period: Duration::from_secs(5),
            dependency_timeout: Duration::from_secs(30),
            logging: LogOptions::default(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ServiceManagerError {
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
    #[error("service '{service}' uses exponential backoff, which requires v0.2")]
    UnsupportedBackoff { service: String },
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
            if service.restart.backoff == BackoffType::Exponential {
                return Err(ServiceManagerError::UnsupportedBackoff {
                    service: name.into(),
                });
            }
            validate_duration(
                format!("services.{name}.restart.initial-delay"),
                service.restart.initial_delay,
            )?;
            let checker = service
                .healthcheck
                .as_ref()
                .map(HealthChecker::new)
                .transpose()
                .map_err(|source| ServiceManagerError::Health {
                    service: name.into(),
                    source,
                })?;
            checkers.insert(name.into(), checker);
        }
        let initial = RuntimeSnapshot {
            supervisor_pid: std::process::id(),
            services: graph
                .service_names()
                .map(|name| (name.into(), ServiceSnapshot::default()))
                .collect(),
        };
        let (snapshots, _) = watch::channel(initial);
        let logs = LogCollector::new(options.logging)?;
        Ok(Self {
            config,
            options,
            layers,
            checkers,
            snapshots,
            logs,
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

    /// Install SIGINT (Ctrl+C) and SIGTERM handlers before spawning children.
    pub async fn run(self) -> Result<RuntimeSnapshot, ServiceManagerError> {
        let mut interrupt = signal(SignalKind::interrupt()).map_err(ServiceManagerError::Signal)?;
        let mut terminate = signal(SignalKind::terminate()).map_err(ServiceManagerError::Signal)?;
        self.run_until(async move {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            }
        })
        .await
    }

    /// Run in the foreground until cancellation, terminal failure, or all service
    /// tasks finish. Shutdown quiesces all actors, then stops reverse graph layers.
    /// State writes use atomic replacement and a nonblocking exclusive file lock.
    pub async fn run_until(
        self,
        shutdown: impl Future<Output = ()>,
    ) -> Result<RuntimeSnapshot, ServiceManagerError> {
        let store = Arc::new(StateStore::open(&self.options.state_path).await?);
        let initial = self.snapshots.borrow().clone();
        store.write(&initial).await?;
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
        for (name, checker) in self.checkers {
            let (control, receiver) = watch::channel(if shutdown_requested {
                Control::Quiescing
            } else {
                Control::Running
            });
            controls.insert(name.clone(), control);
            let actor = ServiceTask::new(
                name.clone(),
                self.config.services[&name].clone(),
                checker,
                self.options.clone(),
                receiver,
                self.snapshots.clone(),
                self.logs.clone(),
            );
            let lease = store.clone();
            let handle = tasks.spawn(async move {
                let _lease = lease;
                actor.run().await
            });
            names.insert(handle.id(), name);
        }
        let mut error = None;
        while !shutdown_requested && !tasks.is_empty() {
            tokio::select! {
                biased;
                _ = &mut shutdown => break,
                _ = updates.changed() => {
                    let snapshot = updates.borrow_and_update().clone();
                    if let Err(failure) = store.write(&snapshot).await {
                        error = Some(failure);
                        break;
                    }
                    if snapshot.services.values().any(|s| s.status == ServiceState::Failed) {
                        break;
                    }
                },
                Some(result) = tasks.join_next_with_id() => {
                    if let Some(failure) = collect_task(result, &mut names, &self.snapshots) {
                        error = Some(failure);
                        break;
                    }
                },
            }
        }
        for control in controls.values() {
            control.send_replace(Control::Quiescing);
        }
        for layer in self.layers.iter().rev() {
            for name in layer {
                controls[name].send_replace(Control::Stopping);
            }
            while names.values().any(|name| layer.contains(name)) {
                if let Some(result) = tasks.join_next_with_id().await {
                    if let Some(failure) = collect_task(result, &mut names, &self.snapshots) {
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
                    service.last_error = Some(error.to_string());
                });
            }
            Some(ServiceManagerError::Task(error))
        }
    }
}
