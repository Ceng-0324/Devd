use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::config::{DependencyCondition, ServiceConfig};

use super::service_manager::{RuntimeSnapshot, ServiceSnapshot, ServiceState};

/// The identity consumed by one dependent generation, not a transient watch
/// event: coalescing state updates must not hide a fast dependency restart.
#[derive(Debug, PartialEq, Eq)]
struct Generation {
    pid: Option<u32>,
    started_at: Option<DateTime<Utc>>,
    restart_count: u32,
}

impl From<&ServiceSnapshot> for Generation {
    fn from(state: &ServiceSnapshot) -> Self {
        Self {
            pid: state.pid,
            started_at: state.started_at,
            restart_count: state.restart_count,
        }
    }
}

#[derive(Default)]
pub(super) struct DependencyRecovery {
    baseline: BTreeMap<String, (DependencyCondition, Generation)>,
}

impl DependencyRecovery {
    /// Capture under the same snapshot borrow that passed startup readiness.
    pub fn capture(config: &ServiceConfig, snapshot: &RuntimeSnapshot) -> Self {
        Self {
            baseline: config
                .depends_on
                .iter()
                .filter(|_| config.restart_on_dep_recovery)
                .map(|edge| {
                    (
                        edge.service.clone(),
                        (
                            edge.condition.clone(),
                            Generation::from(&snapshot.services[&edge.service]),
                        ),
                    )
                })
                .collect(),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.baseline.is_empty()
    }

    /// Wait for every direct dependency before interrupting a running service.
    /// A new generation captures fresh baselines, coalescing recoveries seen
    /// during cleanup, backoff, and startup readiness into that one restart.
    pub fn recovered(&self, snapshot: &RuntimeSnapshot) -> Vec<String> {
        let mut recovered = Vec::new();
        for (name, (condition, previous)) in &self.baseline {
            let current = &snapshot.services[name];
            if !dependency_ready(current, condition) {
                return Vec::new();
            }
            if Generation::from(current) != *previous {
                recovered.push(name.clone());
            }
        }
        recovered
    }
}

pub(super) fn dependency_ready(state: &ServiceSnapshot, condition: &DependencyCondition) -> bool {
    state.pid.is_some()
        && match condition {
            DependencyCondition::Started => matches!(
                state.status,
                ServiceState::Running | ServiceState::Healthy | ServiceState::Unhealthy
            ),
            DependencyCondition::HttpReady
            | DependencyCondition::TcpReady
            | DependencyCondition::SocketReady => state.status == ServiceState::Healthy,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(condition: DependencyCondition) -> (ServiceConfig, RuntimeSnapshot) {
        let mut config: ServiceConfig =
            serde_yaml::from_str("command: test\nrestart-on-dep-recovery: true\ndepends-on: [db]")
                .unwrap();
        config.depends_on[0].condition = condition;
        let snapshot = RuntimeSnapshot {
            supervisor_pid: 1,
            services: [(
                "db".into(),
                ServiceSnapshot {
                    status: ServiceState::Healthy,
                    pid: Some(100),
                    started_at: Some(Utc::now()),
                    ..Default::default()
                },
            )]
            .into(),
        };
        (config, snapshot)
    }

    #[test]
    fn test_dependency_recovery_ignores_health_flaps_and_initial_readiness() {
        for condition in [
            DependencyCondition::Started,
            DependencyCondition::HttpReady,
            DependencyCondition::TcpReady,
            DependencyCondition::SocketReady,
        ] {
            let (config, mut snapshot) = fixture(condition);
            let recovery = DependencyRecovery::capture(&config, &snapshot);
            for status in [
                ServiceState::Running,
                ServiceState::Unhealthy,
                ServiceState::Healthy,
            ] {
                snapshot.services.get_mut("db").unwrap().status = status;
                assert!(recovery.recovered(&snapshot).is_empty());
            }
        }
    }

    #[test]
    fn test_dependency_recovery_requires_replacement_and_matching_readiness() {
        for condition in [
            DependencyCondition::Started,
            DependencyCondition::HttpReady,
            DependencyCondition::TcpReady,
            DependencyCondition::SocketReady,
        ] {
            let (config, mut snapshot) = fixture(condition.clone());
            let recovery = DependencyRecovery::capture(&config, &snapshot);
            // Simulate a completely missed stop/start pair in the watch channel.
            // Even PID reuse and restart-count saturation retain the new start time.
            snapshot.services.get_mut("db").unwrap().started_at =
                Some(Utc::now() + chrono::Duration::seconds(1));
            for status in [
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
                snapshot.services.get_mut("db").unwrap().status = status;
                let expected = if condition == DependencyCondition::Started {
                    matches!(
                        status,
                        ServiceState::Running | ServiceState::Healthy | ServiceState::Unhealthy
                    )
                } else {
                    status == ServiceState::Healthy
                };
                assert_eq!(
                    !recovery.recovered(&snapshot).is_empty(),
                    expected,
                    "{condition:?} {status:?}"
                );
            }
            let state = snapshot.services.get_mut("db").unwrap();
            state.status = ServiceState::Healthy;
            state.pid = None;
            assert!(recovery.recovered(&snapshot).is_empty());
        }
    }

    #[test]
    fn test_dependency_recovery_coalesces_ready_dependencies_and_refreshes_baseline() {
        let (mut config, mut snapshot) = fixture(DependencyCondition::HttpReady);
        config.depends_on.push(crate::config::Dependency {
            service: "cache".into(),
            condition: DependencyCondition::Started,
        });
        snapshot
            .services
            .insert("cache".into(), snapshot.services["db"].clone());
        let recovery = DependencyRecovery::capture(&config, &snapshot);
        snapshot.services.get_mut("cache").unwrap().restart_count += 1;
        snapshot.services.get_mut("db").unwrap().status = ServiceState::Unhealthy;
        assert!(recovery.recovered(&snapshot).is_empty());
        let db = snapshot.services.get_mut("db").unwrap();
        db.restart_count += 2;
        db.status = ServiceState::Healthy;
        assert_eq!(recovery.recovered(&snapshot), ["cache", "db"]);
        let fresh = DependencyRecovery::capture(&config, &snapshot);
        assert!(fresh.recovered(&snapshot).is_empty());
        config.restart_on_dep_recovery = false;
        let disabled = DependencyRecovery::capture(&config, &snapshot);
        snapshot.services.get_mut("db").unwrap().restart_count += 1;
        assert!(!disabled.enabled());
        assert!(disabled.recovered(&snapshot).is_empty());
    }
}
