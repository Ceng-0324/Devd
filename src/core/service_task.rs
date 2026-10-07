#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::{collections::BTreeMap, future::pending, io, process::ExitStatus, time::Duration};

use chrono::Utc;
use tokio::{sync::watch, task::JoinSet};

use crate::config::{
    BackoffType, ResourceLimitAction, RestartPolicy, RestartPolicyType, ServiceConfig,
};
use crate::logging::{LogCollector, LogLevel};

use super::{
    dependency_recovery::{dependency_ready, DependencyRecovery},
    events::{
        DependencyChange, EventData, EventRecorder, ProbeEvidence, RestartCause, RestartOutcome,
    },
    health_check::{HealthChecker, HealthMonitor, HealthState, ProbeResult},
    process_manager::ManagedProcess,
    service_manager::{ManagerOptions, RuntimeSnapshot, ServiceSnapshot, ServiceState},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Control {
    Running,
    Quiescing,
    Stopping(Option<u64>),
    Restarting(u64),
}

pub(super) struct ServiceTask {
    name: String,
    config: ServiceConfig,
    checker: Option<HealthChecker>,
    options: ManagerOptions,
    control: watch::Receiver<Control>,
    snapshots: watch::Sender<RuntimeSnapshot>,
    logs: LogCollector,
    events: EventRecorder,
    cause: Option<u64>,
    state: ServiceSnapshot,
}

enum GenerationEnd {
    Exit(ExitStatus),
    HealthFailure,
    DependencyRecovery(Vec<DependencyChange>),
    ResourceLimit(String),
    Error(super::process_manager::ProcessError),
    Shutdown,
}

impl ServiceTask {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        config: ServiceConfig,
        checker: Option<HealthChecker>,
        options: ManagerOptions,
        control: watch::Receiver<Control>,
        snapshots: watch::Sender<RuntimeSnapshot>,
        logs: LogCollector,
        events: EventRecorder,
        cause: Option<u64>,
    ) -> Self {
        let state = snapshots.borrow().services[&name].clone();
        Self {
            name,
            config,
            checker,
            options,
            control,
            snapshots,
            logs,
            events,
            cause,
            state,
        }
    }

    pub async fn run(mut self) {
        let mut restarting = false;
        loop {
            // A fresh identity for every attempt, including dependency waits
            // and failed spawns; retry counters can saturate and PIDs can recur.
            let generation = self.record(EventData::GenerationPending);
            self.state.event_generation = Some(generation);
            self.cause = Some(generation);
            self.publish();
            let dependencies = match self.wait_dependencies().await {
                Ok(Some(dependencies)) => dependencies,
                Ok(None) => {
                    self.wait_stop().await;
                    self.mark_stopped();
                    break;
                }
                Err(error) => {
                    self.fail(error);
                    break;
                }
            };
            if restarting {
                self.state.restart_count = self.state.restart_count.saturating_add(1);
            }
            self.record(EventData::Starting);
            self.transition(ServiceState::Starting);
            // A stop request must also be able to interrupt async env-file reads.
            let spawn = tokio::select! {
                biased;
                _ = self.control.changed() => None,
                result = ManagedProcess::spawn(&self.name, &self.config) => Some(result),
            };
            let mut process = match spawn {
                None => {
                    self.wait_stop().await;
                    self.mark_stopped();
                    break;
                }
                Some(Ok(process)) => process,
                Some(Err(error)) => {
                    self.cause = Some(self.record(EventData::SpawnFailed {
                        failure: (&error).into(),
                    }));
                    self.trigger(RestartCause::SpawnFailure);
                    self.state.last_error = Some(error.to_string());
                    if !self.schedule_restart(true).await {
                        break;
                    }
                    restarting = true;
                    continue;
                }
            };
            self.state.pid = process.pid();
            self.state.started_at = Some(Utc::now());
            self.state.consecutive_failures = 0;
            self.state.last_error = None;
            let mut readers = self.drain_output(&mut process);
            self.record(EventData::Started {
                pid: self.state.pid,
            });
            self.transition(ServiceState::Running);
            self.cause = None;
            let outcome = self.monitor(&mut process, dependencies).await;
            if let GenerationEnd::ResourceLimit(ref reason) = outcome {
                self.trigger(RestartCause::ResourceLimit {
                    evidence: self.state.resource_restart_evidence.clone(),
                });
                let exhausted = self.state.restart_count >= self.config.restart.max_attempts;
                let message = if exhausted {
                    format!(
                        "resource restart budget exhausted (max-attempts: {}); {reason}",
                        self.config.restart.max_attempts
                    )
                } else {
                    format!("restarting after {reason}")
                };
                self.logs.record(
                    &self.name,
                    self.state.restart_count,
                    if exhausted {
                        LogLevel::Error
                    } else {
                        LogLevel::Warn
                    },
                    message.clone(),
                    false,
                );
                self.state.last_error = Some(message);
            }
            if let GenerationEnd::DependencyRecovery(ref names) = outcome {
                self.trigger(RestartCause::DependencyRecovery {
                    services: names.clone(),
                });
                let reason = format!(
                    "dependencies recovered in new process generations: {}",
                    names
                        .iter()
                        .map(|change| change.service.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                self.state.last_error = Some(
                    if self.state.restart_count >= self.config.restart.max_attempts {
                        format!(
                            "dependency restart budget exhausted (max-attempts: {}); {reason}",
                            self.config.restart.max_attempts
                        )
                    } else {
                        format!("restarting after {reason}")
                    },
                );
            }
            let failed = match outcome {
                GenerationEnd::Shutdown => {
                    self.wait_stop().await;
                    self.transition(ServiceState::Stopping);
                    let stopped = process.stop(self.options.grace_period).await;
                    readers.finish().await;
                    match stopped {
                        Ok(status) => {
                            self.record_exit(status);
                            self.mark_stopped();
                        }
                        Err(error) => {
                            self.cause = Some(self.record(EventData::ProcessFailed {
                                failure: (&error).into(),
                            }));
                            self.fail(error.to_string());
                        }
                    }
                    break;
                }
                GenerationEnd::Exit(status) => {
                    self.record_exit(status);
                    self.trigger(RestartCause::ProcessExit);
                    if !status.success() {
                        self.state.last_error = Some(format!("process exited with {status}"));
                    }
                    !status.success()
                }
                GenerationEnd::HealthFailure
                | GenerationEnd::DependencyRecovery(_)
                | GenerationEnd::ResourceLimit(_) => {
                    self.transition(ServiceState::Stopping);
                    match process.stop(self.options.grace_period).await {
                        Ok(status) => self.record_exit(status),
                        Err(error) => {
                            self.cause = Some(self.record(EventData::ProcessFailed {
                                failure: (&error).into(),
                            }));
                            readers.finish().await;
                            self.fail(error.to_string());
                            break;
                        }
                    }
                    true
                }
                GenerationEnd::Error(error) => {
                    self.cause = Some(self.record(EventData::ProcessFailed {
                        failure: (&error).into(),
                    }));
                    // Drop still kills the group if explicit cleanup fails.
                    if let Err(error) = process.stop(self.options.grace_period).await {
                        self.record(EventData::ProcessFailed {
                            failure: (&error).into(),
                        });
                    }
                    readers.finish().await;
                    self.fail(error.to_string());
                    break;
                }
            };
            readers.finish().await;
            drop(process);
            if !self.schedule_restart(failed).await {
                break;
            }
            restarting = true;
        }
    }

    fn publish(&self) {
        self.snapshots.send_if_modified(|snapshot| {
            let previous = snapshot.services.get_mut(&self.name).unwrap();
            let mut next = self.state.clone();
            // The monitor owns metrics; lifecycle updates preserve only samples
            // belonging to the same live process generation.
            if next.pid.is_some()
                && next.pid == previous.pid
                && next.started_at == previous.started_at
                && next.restart_count == previous.restart_count
                && next.event_generation == previous.event_generation
            {
                next.resources = previous.resources.clone();
                next.resource_restart_reason = previous.resource_restart_reason.clone();
                next.resource_restart_evidence = previous.resource_restart_evidence.clone();
                next.resource_restart_event = previous.resource_restart_event;
            } else {
                next.resources = None;
                next.resource_restart_reason = None;
                next.resource_restart_evidence = None;
                next.resource_restart_event = None;
            }
            if *previous == next {
                false
            } else {
                *previous = next;
                true
            }
        });
    }

    fn record(&self, data: EventData) -> u64 {
        self.events.record(
            Some(&self.name),
            self.state.event_generation,
            self.cause,
            data,
        )
    }

    fn trigger(&mut self, reason: RestartCause) {
        self.cause = Some(self.record(EventData::RestartTriggered {
            reason,
            policy: (&self.config.restart).into(),
        }));
    }

    fn transition(&mut self, state: ServiceState) {
        if self.state.status != state {
            self.record(EventData::StateChanged {
                from: self.state.status,
                to: state,
            });
        }
        self.state.status = state;
        self.publish();
    }

    fn fail(&mut self, error: String) {
        self.state.pid = None;
        self.state.last_error = Some(error);
        self.transition(ServiceState::Failed);
    }

    fn record_exit(&mut self, status: ExitStatus) {
        self.state.pid = None;
        self.state.last_exit_code = status.code();
        #[cfg(unix)]
        {
            self.state.last_exit_signal = status.signal();
        }
        #[cfg(windows)]
        {
            self.state.last_exit_signal = None;
        }
        self.cause = Some(self.record(EventData::Exited {
            code: self.state.last_exit_code,
            signal: self.state.last_exit_signal,
        }));
        self.publish();
    }

    fn running(&self) -> bool {
        *self.control.borrow() == Control::Running
    }

    fn mark_stopped(&mut self) {
        self.transition(
            if matches!(*self.control.borrow(), Control::Restarting(_)) {
                ServiceState::Restarting
            } else {
                ServiceState::Stopped
            },
        );
    }

    async fn wait_stop(&mut self) {
        while !matches!(
            *self.control.borrow_and_update(),
            Control::Stopping(_) | Control::Restarting(_)
        ) {
            if self.control.changed().await.is_err() {
                break;
            }
        }
        let cause = match *self.control.borrow() {
            Control::Restarting(cause) => Some(cause),
            Control::Stopping(cause) => cause,
            _ => None,
        };
        self.cause = Some(self.events.record(
            Some(&self.name),
            self.state.event_generation,
            cause,
            EventData::ServiceStopRequested {
                manual_restart: matches!(*self.control.borrow(), Control::Restarting(_)),
            },
        ));
    }

    async fn wait_dependencies(&mut self) -> Result<Option<DependencyRecovery>, String> {
        let mut updates = self.snapshots.subscribe();
        let deadline = tokio::time::Instant::now() + self.options.dependency_timeout;
        let mut waiting = BTreeMap::new();
        loop {
            if !self.running() {
                return Ok(None);
            }
            {
                let snapshot = updates.borrow_and_update();
                let mut ready = true;
                for edge in &self.config.depends_on {
                    let dependency = &snapshot.services[&edge.service];
                    if matches!(
                        dependency.status,
                        ServiceState::Failed | ServiceState::Stopped
                    ) {
                        self.cause = Some(self.record(EventData::DependencyFailed {
                            service: edge.service.clone(),
                            state: dependency.status,
                        }));
                        return Err(format!(
                            "dependency '{}' is {:?}",
                            edge.service, dependency.status
                        ));
                    }
                    let edge_ready = dependency_ready(dependency, &edge.condition);
                    if !edge_ready && !waiting.contains_key(&edge.service) {
                        let event = self.record(EventData::DependencyWaiting {
                            service: edge.service.clone(),
                            condition: edge.condition.clone(),
                            observed_generation: dependency.event_generation,
                            timeout: self.options.dependency_timeout,
                            remaining: deadline
                                .saturating_duration_since(tokio::time::Instant::now()),
                        });
                        waiting.insert(edge.service.clone(), event);
                    } else if edge_ready && waiting.contains_key(&edge.service) {
                        let cause = waiting.remove(&edge.service);
                        self.events.record(
                            Some(&self.name),
                            self.state.event_generation,
                            cause,
                            EventData::DependencyReady {
                                service: edge.service.clone(),
                                condition: edge.condition.clone(),
                                observed_generation: dependency.event_generation,
                            },
                        );
                    }
                    ready &= edge_ready;
                }
                if ready {
                    return Ok(Some(DependencyRecovery::capture(&self.config, &snapshot)));
                }
            }
            tokio::select! {
                biased;
                _ = self.control.changed() => return Ok(None),
                _ = tokio::time::sleep_until(deadline) => {
                    self.cause = Some(self.record(EventData::DependencyTimedOut { timeout: self.options.dependency_timeout }));
                    return Err(format!("dependency readiness timed out after {:?}", self.options.dependency_timeout));
                },
                _ = updates.changed() => {},
            }
        }
    }

    async fn monitor(
        &mut self,
        process: &mut ManagedProcess,
        dependencies: DependencyRecovery,
    ) -> GenerationEnd {
        let mut health = self.checker.clone().map(HealthMonitor::new);
        let mut previous_health = None;
        let mut health_event = None;
        let mut updates = self.snapshots.subscribe();
        let resource_restarts = self
            .config
            .limits
            .as_ref()
            .is_some_and(|limits| limits.on_exceed == ResourceLimitAction::Restart);
        loop {
            // A resource/peer update is not cancellation of this service's
            // probe. Keep the in-flight check alive across dependency wakes.
            let probe = async {
                match &mut health {
                    Some(monitor) => monitor.next_check().await,
                    None => pending().await,
                }
            };
            tokio::pin!(probe);
            let observation = loop {
                if !self.running() {
                    return GenerationEnd::Shutdown;
                }
                let (recovered, resource_reason, resource_event) = {
                    let snapshot = updates.borrow_and_update();
                    let current = &snapshot.services[&self.name];
                    let reason = (resource_restarts
                        && current.pid == self.state.pid
                        && current.started_at == self.state.started_at
                        && current.restart_count == self.state.restart_count
                        && current.event_generation == self.state.event_generation)
                        .then(|| current.resource_restart_reason.clone())
                        .flatten();
                    if reason.is_some() {
                        self.state.resource_restart_evidence =
                            current.resource_restart_evidence.clone();
                    }
                    (
                        dependencies.recovered(&snapshot),
                        reason,
                        current.resource_restart_event,
                    )
                };
                tokio::select! {
                    biased;
                    _ = self.control.changed() => return GenerationEnd::Shutdown,
                    result = process.wait() => return match result {
                        Ok(status) => GenerationEnd::Exit(status),
                        Err(error) => GenerationEnd::Error(error),
                    },
                    _ = std::future::ready(()), if !recovered.is_empty() => {
                        return GenerationEnd::DependencyRecovery(recovered);
                    },
                    _ = std::future::ready(()), if resource_reason.is_some() => {
                        self.cause = resource_event;
                        return GenerationEnd::ResourceLimit(resource_reason.unwrap());
                    },
                    observation = &mut probe => break observation,
                    _ = updates.changed(), if dependencies.enabled() || resource_restarts => {},
                }
            };
            self.state.consecutive_failures = observation.consecutive_failures;
            let failure = match &observation.result {
                ProbeResult::Healthy => None,
                ProbeResult::Unhealthy(error) => Some(ProbeEvidence::from(error)),
            };
            let next_health = (observation.state, failure.clone());
            if previous_health.as_ref() != Some(&next_health) {
                health_event = Some(self.events.record(
                    Some(&self.name),
                    self.state.event_generation,
                    None,
                    EventData::HealthChanged {
                        state: match observation.state {
                            HealthState::Healthy => ServiceState::Healthy,
                            HealthState::Retrying => ServiceState::Running,
                            HealthState::Unhealthy => ServiceState::Unhealthy,
                        },
                        consecutive_failures: observation.consecutive_failures,
                        failure,
                    },
                ));
                previous_health = Some(next_health);
            }
            self.state.last_error = match observation.result {
                ProbeResult::Healthy => None,
                ProbeResult::Unhealthy(error) => Some(error.to_string()),
            };
            self.cause = health_event;
            self.transition(match observation.state {
                HealthState::Healthy => ServiceState::Healthy,
                HealthState::Retrying => ServiceState::Running,
                HealthState::Unhealthy => ServiceState::Unhealthy,
            });
            if observation.state == HealthState::Unhealthy
                && self.config.restart.policy != RestartPolicyType::Never
            {
                self.trigger(RestartCause::HealthFailure);
                return GenerationEnd::HealthFailure;
            }
            self.cause = None;
        }
    }

    async fn schedule_restart(&mut self, failed: bool) -> bool {
        if !self.running() {
            self.wait_stop().await;
            self.mark_stopped();
            return false;
        }
        let retry = should_restart(&self.config, failed, self.state.restart_count);
        let policy_allows = match self.config.restart.policy {
            RestartPolicyType::Always => true,
            RestartPolicyType::OnFailure => failed,
            RestartPolicyType::Never => false,
        };
        let outcome = if retry {
            RestartOutcome::Scheduled
        } else if policy_allows {
            RestartOutcome::BudgetExhausted
        } else {
            RestartOutcome::PolicyDeclined
        };
        self.cause = Some(self.record(EventData::RestartDecision {
            outcome,
            policy: (&self.config.restart).into(),
            restart_count: self.state.restart_count,
            delay: retry.then(|| restart_delay(&self.config.restart, self.state.restart_count)),
        }));
        if !retry {
            self.transition(if failed {
                ServiceState::Failed
            } else {
                ServiceState::Stopped
            });
            return false;
        }
        self.transition(ServiceState::Restarting);
        tokio::select! {
            biased;
            _ = self.control.changed() => {
                self.wait_stop().await;
                self.mark_stopped();
                false
            },
            _ = tokio::time::sleep(restart_delay(&self.config.restart, self.state.restart_count)) => true,
        }
    }

    fn drain_output(&self, process: &mut ManagedProcess) -> OutputReaders {
        let mut tasks = JoinSet::new();
        if let Some(stdout) = process.take_stdout() {
            let (logs, name, generation) = (
                self.logs.clone(),
                self.name.clone(),
                self.state.restart_count,
            );
            tasks
                .spawn(async move { logs.collect(stdout, name, generation, LogLevel::Info).await });
        }
        if let Some(stderr) = process.take_stderr() {
            let (logs, name, generation) = (
                self.logs.clone(),
                self.name.clone(),
                self.state.restart_count,
            );
            tasks.spawn(async move {
                logs.collect(stderr, name, generation, LogLevel::Error)
                    .await
            });
        }
        OutputReaders(tasks)
    }
}

fn restart_delay(policy: &RestartPolicy, count: u32) -> Duration {
    let mut delay = policy.initial_delay;
    if policy.backoff == BackoffType::Fixed || delay.is_zero() {
        return delay;
    }
    // At most 94 doublings span Duration's range, even for unlimited retries.
    for _ in 0..count {
        if delay >= policy.max_delay {
            return policy.max_delay;
        }
        delay = delay
            .checked_mul(2)
            .unwrap_or(policy.max_delay)
            .min(policy.max_delay);
    }
    delay
}

fn should_restart(config: &ServiceConfig, failed: bool, count: u32) -> bool {
    count < config.restart.max_attempts
        && match config.restart.policy {
            RestartPolicyType::Always => true,
            RestartPolicyType::OnFailure => failed,
            RestartPolicyType::Never => false,
        }
}

struct OutputReaders(JoinSet<io::Result<()>>);

impl OutputReaders {
    async fn finish(&mut self) {
        // Escaped descendants can retain a pipe; do not let log EOF hang shutdown.
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while self.0.join_next().await.is_some() {}
        })
        .await;
        self.0.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resources_survive_health_updates_and_clear_on_exit() {
        use crate::core::resource_monitor::ResourceUsage;
        let config: ServiceConfig = serde_yaml::from_str("command: sleep 60").unwrap();
        let (_, control) = watch::channel(Control::Running);
        let initial = ServiceSnapshot {
            pid: Some(123),
            started_at: Some(Utc::now()),
            status: ServiceState::Running,
            ..Default::default()
        };
        let (snapshots, _) = watch::channel(RuntimeSnapshot {
            supervisor_pid: std::process::id(),
            event_run_id: None,
            services: [("child".into(), initial)].into(),
        });
        let mut task = ServiceTask::new(
            "child".into(),
            config,
            None,
            ManagerOptions::new("unused"),
            control,
            snapshots.clone(),
            LogCollector::new(Default::default()).unwrap(),
            EventRecorder::new("unused".into(), None),
            None,
        );
        let usage = ResourceUsage {
            cpu_percent: Some(50.0),
            memory_bytes: 100_000,
            sampled_at: Utc::now(),
        };
        snapshots.send_modify(|s| {
            let service = s.services.get_mut("child").unwrap();
            service.resources = Some(usage.clone());
            service.resource_restart_reason = Some("RSS limit exceeded".into());
        });
        task.transition(ServiceState::Healthy);
        assert_eq!(snapshots.borrow().services["child"].resources, Some(usage));
        assert_eq!(
            snapshots.borrow().services["child"]
                .resource_restart_reason
                .as_deref(),
            Some("RSS limit exceeded")
        );
        task.fail("process observation failed".into());
        assert!(snapshots.borrow().services["child"].resources.is_none());
        assert!(snapshots.borrow().services["child"]
            .resource_restart_reason
            .is_none());
        assert!(snapshots.borrow().services["child"].pid.is_none());
    }

    #[test]
    fn test_restart_delay_growth_cap_zero_and_overflow() {
        let mut policy = RestartPolicy {
            backoff: BackoffType::Exponential,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(35),
            ..RestartPolicy::default()
        };
        for (count, millis) in [(0, 10), (1, 20), (2, 35), (u32::MAX, 35)] {
            assert_eq!(restart_delay(&policy, count), Duration::from_millis(millis));
        }
        policy.initial_delay = Duration::ZERO;
        assert_eq!(restart_delay(&policy, u32::MAX), Duration::ZERO);
        policy.initial_delay = Duration::from_nanos(1);
        policy.max_delay = Duration::MAX;
        assert_eq!(restart_delay(&policy, u32::MAX), Duration::MAX);
        policy.backoff = BackoffType::Fixed;
        policy.initial_delay = Duration::from_secs(60);
        policy.max_delay = Duration::ZERO;
        assert_eq!(restart_delay(&policy, u32::MAX), Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn test_restart_wait_uses_cumulative_count_and_is_cancellable() {
        let config: ServiceConfig = serde_yaml::from_str(
            "command: sleep 60\nrestart: {backoff: exponential, initial-delay: 1s, max-delay: 3s, max-attempts: 10}",
        )
        .unwrap();
        let (control, receiver) = watch::channel(Control::Running);
        let (snapshots, _) = watch::channel(RuntimeSnapshot {
            supervisor_pid: std::process::id(),
            event_run_id: None,
            services: [("child".into(), ServiceSnapshot::default())].into(),
        });
        let mut task = ServiceTask::new(
            "child".into(),
            config,
            None,
            ManagerOptions::new("unused"),
            receiver,
            snapshots,
            LogCollector::new(Default::default()).unwrap(),
            EventRecorder::new("unused".into(), None),
            None,
        );
        for (count, seconds) in [(0, 1), (1, 2), (2, 3), (3, 3)] {
            task.state.restart_count = count;
            let start = tokio::time::Instant::now();
            assert!(task.schedule_restart(true).await);
            assert_eq!(start.elapsed(), Duration::from_secs(seconds));
        }
        let start = tokio::time::Instant::now();
        let stop = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            control.send_replace(Control::Stopping(None));
        };
        let (retry, _) = tokio::join!(task.schedule_restart(true), stop);
        assert!(!retry);
        assert_eq!(start.elapsed(), Duration::from_millis(100));
        assert_eq!(task.state.status, ServiceState::Stopped);
    }

    #[test]
    fn test_orchestration_restart_policy_and_attempt_budget() {
        let mut config: ServiceConfig = serde_yaml::from_str("command: test").unwrap();
        for (policy, clean, failed) in [
            (RestartPolicyType::Never, false, false),
            (RestartPolicyType::OnFailure, false, true),
            (RestartPolicyType::Always, true, true),
        ] {
            config.restart.policy = policy;
            assert_eq!(should_restart(&config, false, 0), clean);
            assert_eq!(should_restart(&config, true, 0), failed);
            assert!(!should_restart(&config, true, config.restart.max_attempts));
        }
        config.restart.max_attempts = 0;
        assert!(!should_restart(&config, true, 0));
    }
}
