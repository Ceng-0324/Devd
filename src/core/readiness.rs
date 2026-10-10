//! Readiness observations; no probes, process control, or disk state access.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::service_manager::{RuntimeSnapshot, ServiceState};

/// Runtime-only control barriers. The epoch survives completed/no-op reloads.
#[derive(Debug, Clone, Default)]
pub(crate) struct ControlState {
    pub reload_epoch: u64,
    pub reloading: bool,
    pub stopping: bool,
    pub restarting: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Outcome {
    Waiting,
    Ready,
    Failed,
    Stopping,
    Reloaded,
    TimedOut,
    Cancelled,
    Disconnected,
    Unavailable,
    InvalidService,
    Busy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Evidence {
    pub requires_healthy: bool,
    pub status: Option<ServiceState>,
    pub pid: Option<u32>,
    pub generation: Option<u64>,
    pub ready: bool,
    pub restart_pending: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Observation {
    pub outcome: Outcome,
    pub services: BTreeMap<String, Evidence>,
    pub blocking: Vec<String>,
}

pub(crate) fn observe(
    selected: &BTreeMap<String, bool>,
    snapshot: &RuntimeSnapshot,
    control: &ControlState,
    epoch: u64,
) -> Observation {
    let services: BTreeMap<_, _> = selected
        .iter()
        .map(|(name, &requires_healthy)| {
            let state = snapshot.services.get(name);
            let restart_pending = control.restarting.contains(name);
            let ready = !restart_pending
                && state.is_some_and(|state| {
                    state.pid.is_some()
                        && if requires_healthy {
                            state.status == ServiceState::Healthy
                        } else {
                            state.status == ServiceState::Running
                        }
                });
            (
                name.clone(),
                Evidence {
                    requires_healthy,
                    status: state.map(|s| s.status),
                    pid: state.and_then(|s| s.pid),
                    generation: state.and_then(|s| s.event_generation),
                    ready,
                    restart_pending,
                    last_error: state.and_then(|s| s.last_error.clone()),
                },
            )
        })
        .collect();
    let blocking: Vec<_> = services
        .iter()
        .filter(|(_, s)| !s.ready)
        .map(|(n, _)| n.clone())
        .collect();
    let outcome = if control.stopping {
        Outcome::Stopping
    } else if control.reloading || control.reload_epoch != epoch {
        Outcome::Reloaded
    } else if services.values().any(|s| s.status.is_none()) {
        Outcome::InvalidService
    } else if services.values().any(|s| {
        s.status == Some(ServiceState::Failed)
            || (!s.restart_pending && s.status == Some(ServiceState::Stopped))
    }) {
        Outcome::Failed
    } else if blocking.is_empty() {
        Outcome::Ready
    } else {
        Outcome::Waiting
    };
    Observation {
        outcome,
        services,
        blocking,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::service_manager::ServiceSnapshot;

    fn snapshot(status: ServiceState, pid: Option<u32>) -> RuntimeSnapshot {
        RuntimeSnapshot {
            supervisor_pid: 1,
            event_run_id: Some("run".into()),
            services: BTreeMap::from([(
                "api".into(),
                ServiceSnapshot {
                    status,
                    pid,
                    event_generation: Some(2),
                    ..Default::default()
                },
            )]),
        }
    }

    #[test]
    fn test_readiness_requires_live_pid_and_configured_health() {
        for requires_health in [false, true] {
            for state in [
                ServiceState::Pending,
                ServiceState::Starting,
                ServiceState::Running,
                ServiceState::Healthy,
                ServiceState::Unhealthy,
                ServiceState::Restarting,
                ServiceState::Stopping,
                ServiceState::Stopped,
                ServiceState::Failed,
            ] {
                for pid in [None, Some(42)] {
                    let selected = BTreeMap::from([("api".into(), requires_health)]);
                    let result = observe(
                        &selected,
                        &snapshot(state, pid),
                        &ControlState::default(),
                        0,
                    );
                    let ready = pid.is_some()
                        && state
                            == if requires_health {
                                ServiceState::Healthy
                            } else {
                                ServiceState::Running
                            };
                    assert_eq!(result.services["api"].ready, ready);
                    let expected = if matches!(state, ServiceState::Stopped | ServiceState::Failed)
                    {
                        Outcome::Failed
                    } else if ready {
                        Outcome::Ready
                    } else {
                        Outcome::Waiting
                    };
                    assert_eq!(
                        result.outcome, expected,
                        "{state:?}, {pid:?}, health={requires_health}"
                    );
                    assert_eq!(result.services["api"].generation, Some(2));
                    assert_eq!(result.blocking.is_empty(), ready);
                }
            }
        }
    }

    #[test]
    fn test_readiness_manual_restart_and_reload_barriers() {
        let selected = BTreeMap::from([("api".into(), false)]);
        let mut control = ControlState::default();
        control.restarting.insert("api".into());
        for state in [
            ServiceState::Running,
            ServiceState::Stopped,
            ServiceState::Restarting,
        ] {
            let result = observe(&selected, &snapshot(state, Some(42)), &control, 0);
            assert_eq!(result.outcome, Outcome::Waiting);
            assert!(result.services["api"].restart_pending);
        }
        control.restarting.clear();
        let running = snapshot(ServiceState::Running, Some(43));
        assert_eq!(
            observe(&selected, &running, &control, 0).outcome,
            Outcome::Ready
        );
        control.reload_epoch = 1;
        // Even a reload that has already completed invalidates the wait.
        assert_eq!(
            observe(&selected, &running, &control, 0).outcome,
            Outcome::Reloaded
        );
        control.reloading = true;
        assert_eq!(
            observe(&selected, &running, &control, 1).outcome,
            Outcome::Reloaded
        );
        control.stopping = true;
        assert_eq!(
            observe(&selected, &running, &control, 1).outcome,
            Outcome::Stopping
        );
    }

    #[test]
    fn test_readiness_selection_unknown_names_and_failure_evidence() {
        let mut snapshot = snapshot(ServiceState::Running, Some(42));
        snapshot.services.insert(
            "broken".into(),
            ServiceSnapshot {
                status: ServiceState::Failed,
                last_error: Some("spawn failed".into()),
                ..Default::default()
            },
        );
        let mut selected = BTreeMap::from([("api".into(), false)]);
        let control = ControlState::default();
        assert_eq!(
            observe(&selected, &snapshot, &control, 0).outcome,
            Outcome::Ready
        );
        selected.insert("broken".into(), false);
        let result = observe(&selected, &snapshot, &control, 0);
        assert_eq!(result.outcome, Outcome::Failed);
        assert_eq!(result.blocking, ["broken"]);
        assert_eq!(
            result.services["broken"].last_error.as_deref(),
            Some("spawn failed")
        );
        selected.insert("unknown".into(), false);
        let result = observe(&selected, &snapshot, &control, 0);
        assert_eq!(result.outcome, Outcome::InvalidService);
        assert_eq!(result.blocking, ["broken", "unknown"]);
        assert_eq!(result.services["unknown"].status, None);
    }
}
