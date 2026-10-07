//! Periodic resource samples for service leaders, independent of process control.

use std::{collections::BTreeMap, time::Duration};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::sync::watch;

use crate::{
    config::{ResourceLimitAction, ResourceThresholds},
    logging::{LogCollector, LogLevel},
};

use super::{
    events::{EventData, EventRecorder, ResourceEvidence, ResourceValue},
    service_manager::{RuntimeSnapshot, ServiceSnapshot, ServiceState},
};

const RESTART_SAMPLES: u8 = 3;

/// A service leader's RSS and CPU usage (one fully occupied core is 100%).
/// CPU is unavailable until two successful samples of the same process exist.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResourceUsage {
    pub cpu_percent: Option<f32>,
    pub memory_bytes: u64,
    pub sampled_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Generation {
    pid: u32,
    started_at: DateTime<Utc>,
    restart_count: u32,
    event_generation: Option<u64>,
}

impl Generation {
    fn of(state: &ServiceSnapshot) -> Option<Self> {
        Some(Self {
            pid: state.pid?,
            started_at: state.started_at?,
            restart_count: state.restart_count,
            event_generation: state.event_generation,
        })
    }
}

struct ProcessSampler {
    generation: Generation,
    system: System,
    previous_start: Option<u64>,
}

impl ProcessSampler {
    fn sample(&mut self) -> Option<ResourceUsage> {
        let pid = Pid::from_u32(self.generation.pid);
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .without_tasks(),
        );
        let Some(process) = self.system.process(pid) else {
            self.previous_start = None;
            return None;
        };
        // Missing OS access can produce a process entry with zero RSS. Do not
        // present that as a successful zero-memory measurement.
        if process.memory() == 0 {
            self.previous_start = None;
            return None;
        }
        let start = process.start_time();
        // OS start times have second precision. A PID born after this actor's
        // spawn, or a changed OS identity, no longer identifies its service.
        if start > self.generation.started_at.timestamp().max(0) as u64
            || self
                .previous_start
                .is_some_and(|previous| previous != start)
        {
            self.previous_start = None;
            return None;
        }
        let cpu = process.cpu_usage();
        let cpu_percent =
            (self.previous_start == Some(start) && cpu.is_finite() && cpu >= 0.0).then_some(cpu);
        self.previous_start = Some(start);
        Some(ResourceUsage {
            cpu_percent,
            memory_bytes: process.memory(),
            sampled_at: Utc::now(),
        })
    }
}

#[derive(Default)]
struct Sampler {
    processes: BTreeMap<String, ProcessSampler>,
}

struct Sample {
    generation: Generation,
    resources: Option<ResourceUsage>,
}

impl Sampler {
    fn sample(&mut self, snapshot: &RuntimeSnapshot) -> BTreeMap<String, Sample> {
        // Each System only ever sees one generation's PID. Removing a service
        // drops its OS cache/file handles, so repeated restarts cannot grow it.
        self.processes.retain(|name, sampler| {
            snapshot
                .services
                .get(name)
                .and_then(Generation::of)
                .as_ref()
                == Some(&sampler.generation)
        });
        snapshot
            .services
            .iter()
            .filter_map(|(name, state)| {
                let generation = Generation::of(state)?;
                let sampler =
                    self.processes
                        .entry(name.clone())
                        .or_insert_with(|| ProcessSampler {
                            generation: generation.clone(),
                            system: System::new(),
                            previous_start: None,
                        });
                Some((
                    name.clone(),
                    Sample {
                        generation,
                        resources: sampler.sample(),
                    },
                ))
            })
            .collect()
    }
}

fn apply(snapshot: &mut RuntimeSnapshot, samples: BTreeMap<String, Sample>) -> bool {
    let mut changed = false;
    for (name, sample) in samples {
        if let Some(state) = snapshot.services.get_mut(&name) {
            // A read can finish after exit, restart, or PID reuse. The lifecycle
            // owner is authoritative; a sample may only update its own generation.
            if Generation::of(state).as_ref() == Some(&sample.generation)
                && state.resources != sample.resources
            {
                state.resources = sample.resources;
                changed = true;
            }
        }
    }
    changed
}

#[derive(Default)]
struct Alarms(BTreeMap<String, Alarm>);

struct Alarm {
    generation: Generation,
    cpu: bool,
    memory: bool,
    cpu_samples: u8,
    memory_samples: u8,
    last_sample: Option<DateTime<Utc>>,
}

struct Alert {
    service: String,
    generation: u32,
    level: LogLevel,
    message: String,
}

impl Alarms {
    fn evaluate(
        &mut self,
        snapshot: &mut RuntimeSnapshot,
        limits: &BTreeMap<String, ResourceThresholds>,
        events: Option<&EventRecorder>,
    ) -> Vec<Alert> {
        self.0.retain(|name, alarm| {
            snapshot
                .services
                .get(name)
                .and_then(Generation::of)
                .as_ref()
                == Some(&alarm.generation)
        });
        let mut alerts = Vec::new();
        for (name, thresholds) in limits {
            let Some(state) = snapshot.services.get_mut(name) else {
                continue;
            };
            let Some(generation) = Generation::of(state) else {
                continue;
            };
            if !matches!(
                state.status,
                ServiceState::Running | ServiceState::Healthy | ServiceState::Unhealthy
            ) {
                continue;
            }
            let alarm = self.0.entry(name.clone()).or_insert_with(|| Alarm {
                generation,
                cpu: false,
                memory: false,
                cpu_samples: 0,
                memory_samples: 0,
                last_sample: None,
            });
            let Some(usage) = &state.resources else {
                alarm.cpu_samples = 0;
                alarm.memory_samples = 0;
                continue;
            };
            // A health/peer update or rejected late sample cannot advance a
            // streak. Decisions are latched for this generation until exit.
            if alarm.last_sample == Some(usage.sampled_at) {
                continue;
            }
            alarm.last_sample = Some(usage.sampled_at);
            if let (Some(limit), Some(value)) = (thresholds.cpu_percent, usage.cpu_percent) {
                let exceeded = f64::from(value) > f64::from(limit);
                alarm.cpu_samples = if exceeded {
                    alarm.cpu_samples.saturating_add(1).min(RESTART_SAMPLES)
                } else {
                    0
                };
                if exceeded != alarm.cpu {
                    alarm.cpu = exceeded;
                    if let Some(events) = events {
                        events.record(
                            Some(name),
                            state.event_generation,
                            None,
                            EventData::ResourceChanged {
                                exceeded,
                                evidence: ResourceEvidence {
                                    value: ResourceValue::Cpu {
                                        percent: value,
                                        limit_percent: limit,
                                    },
                                    sampled_at: usage.sampled_at,
                                    consecutive_samples: alarm.cpu_samples,
                                    restart_authorized: thresholds.on_exceed
                                        == ResourceLimitAction::Restart,
                                },
                            },
                        );
                    }
                    alerts.push(Alert {
                        service: name.clone(),
                        generation: state.restart_count,
                        level: if exceeded {
                            LogLevel::Warn
                        } else {
                            LogLevel::Info
                        },
                        message: format!(
                            "CPU {}: {value:.1}% (limit {limit}%)",
                            if exceeded {
                                "limit exceeded"
                            } else {
                                "back within limit"
                            }
                        ),
                    });
                }
            } else {
                alarm.cpu_samples = 0;
            }
            if let Some(limit) = thresholds.memory_bytes {
                let exceeded = usage.memory_bytes > limit;
                alarm.memory_samples = if exceeded {
                    alarm.memory_samples.saturating_add(1).min(RESTART_SAMPLES)
                } else {
                    0
                };
                if exceeded != alarm.memory {
                    alarm.memory = exceeded;
                    if let Some(events) = events {
                        events.record(
                            Some(name),
                            state.event_generation,
                            None,
                            EventData::ResourceChanged {
                                exceeded,
                                evidence: ResourceEvidence {
                                    value: ResourceValue::Memory {
                                        bytes: usage.memory_bytes,
                                        limit_bytes: limit,
                                    },
                                    sampled_at: usage.sampled_at,
                                    consecutive_samples: alarm.memory_samples,
                                    restart_authorized: thresholds.on_exceed
                                        == ResourceLimitAction::Restart,
                                },
                            },
                        );
                    }
                    alerts.push(Alert {
                        service: name.clone(),
                        generation: state.restart_count,
                        level: if exceeded {
                            LogLevel::Warn
                        } else {
                            LogLevel::Info
                        },
                        message: format!(
                            "RSS {}: {} B (limit {limit} B)",
                            if exceeded {
                                "limit exceeded"
                            } else {
                                "back within limit"
                            },
                            usage.memory_bytes
                        ),
                    });
                }
            }
            if thresholds.on_exceed == ResourceLimitAction::Restart
                && state.resource_restart_reason.is_none()
            {
                let reason = if alarm.cpu_samples == RESTART_SAMPLES {
                    Some(format!("CPU limit exceeded for {RESTART_SAMPLES} consecutive samples: {:.1}% (limit {}%)", usage.cpu_percent.unwrap(), thresholds.cpu_percent.unwrap()))
                } else if alarm.memory_samples == RESTART_SAMPLES {
                    Some(format!("RSS limit exceeded for {RESTART_SAMPLES} consecutive samples: {} B (limit {} B)", usage.memory_bytes, thresholds.memory_bytes.unwrap()))
                } else {
                    None
                };
                if reason.is_some() {
                    let evidence = ResourceEvidence {
                        value: if alarm.cpu_samples == RESTART_SAMPLES {
                            ResourceValue::Cpu {
                                percent: usage.cpu_percent.unwrap(),
                                limit_percent: thresholds.cpu_percent.unwrap(),
                            }
                        } else {
                            ResourceValue::Memory {
                                bytes: usage.memory_bytes,
                                limit_bytes: thresholds.memory_bytes.unwrap(),
                            }
                        },
                        sampled_at: usage.sampled_at,
                        consecutive_samples: RESTART_SAMPLES,
                        restart_authorized: true,
                    };
                    if let Some(events) = events {
                        state.resource_restart_event = Some(events.record(
                            Some(name),
                            state.event_generation,
                            None,
                            EventData::ResourceRestartRequested {
                                evidence: evidence.clone(),
                            },
                        ));
                    }
                    state.resource_restart_evidence = Some(evidence);
                }
                state.resource_restart_reason = reason;
            }
        }
        alerts
    }
}

pub(super) async fn run(
    snapshots: watch::Sender<RuntimeSnapshot>,
    logs: LogCollector,
    limits: BTreeMap<String, ResourceThresholds>,
    events: EventRecorder,
) -> ! {
    let mut sampler = Sampler::default();
    let mut alarms = Alarms::default();
    loop {
        let snapshot = snapshots.borrow().clone();
        let result = tokio::task::spawn_blocking(move || {
            let samples = sampler.sample(&snapshot);
            (sampler, samples)
        })
        .await;
        match result {
            Ok((next, samples)) => {
                sampler = next;
                let mut alerts = Vec::new();
                snapshots.send_modify(|snapshot| {
                    apply(snapshot, samples);
                    alerts = alarms.evaluate(snapshot, &limits, Some(&events));
                });
                for alert in alerts {
                    logs.record(
                        &alert.service,
                        alert.generation,
                        alert.level,
                        alert.message,
                        false,
                    );
                }
            }
            Err(_) => {
                // Observation failure must neither stop services nor preserve
                // misleading old values. Retry with fresh baselines next tick.
                sampler = Sampler::default();
                snapshots.send_if_modified(|snapshot| {
                    let mut changed = false;
                    for state in snapshot.services.values_mut() {
                        changed |= state.resources.take().is_some();
                    }
                    // Missing observations break every consecutive streak.
                    alarms.evaluate(snapshot, &limits, Some(&events));
                    changed
                });
            }
        }
        // Slow OS reads must not cause back-to-back CPU measurements.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> RuntimeSnapshot {
        RuntimeSnapshot {
            supervisor_pid: std::process::id(),
            event_run_id: None,
            services: [(
                "worker".into(),
                ServiceSnapshot {
                    status: ServiceState::Running,
                    pid: Some(std::process::id()),
                    started_at: Some(Utc::now()),
                    ..Default::default()
                },
            )]
            .into(),
        }
    }

    fn usage() -> ResourceUsage {
        ResourceUsage {
            cpu_percent: Some(125.0),
            memory_bytes: 42_000,
            sampled_at: Utc::now(),
        }
    }

    #[test]
    fn test_resource_alarms_deduplicate_recover_and_reset_on_restart() {
        let limits = [(
            "worker".into(),
            ResourceThresholds {
                cpu_percent: Some(100),
                memory_bytes: Some(40_000),
                on_exceed: ResourceLimitAction::Warn,
            },
        )]
        .into();
        let mut state = snapshot();
        state.services.get_mut("worker").unwrap().resources = Some(usage());
        let mut alarms = Alarms::default();
        let first = alarms.evaluate(&mut state, &limits, None);
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|alert| alert.level == LogLevel::Warn));
        assert!(alarms.evaluate(&mut state, &limits, None).is_empty());
        state.services.get_mut("worker").unwrap().resources = None;
        assert!(alarms.evaluate(&mut state, &limits, None).is_empty());
        state.services.get_mut("worker").unwrap().resources = Some(ResourceUsage {
            cpu_percent: None,
            ..usage()
        });
        assert!(alarms.evaluate(&mut state, &limits, None).is_empty());
        let service = state.services.get_mut("worker").unwrap();
        service.resources = Some(ResourceUsage {
            cpu_percent: Some(100.0),
            memory_bytes: 40_000,
            sampled_at: Utc::now(),
        });
        let recovered = alarms.evaluate(&mut state, &limits, None);
        assert_eq!(recovered.len(), 2);
        assert!(recovered.iter().all(|alert| alert.level == LogLevel::Info));
        let service = state.services.get_mut("worker").unwrap();
        service.restart_count += 1;
        service.resources = Some(usage());
        assert_eq!(alarms.evaluate(&mut state, &limits, None).len(), 2);
        state.services.get_mut("worker").unwrap().restart_count += 1;
        let restarted = alarms.evaluate(&mut state, &limits, None);
        assert_eq!(restarted.len(), 2);
        assert!(restarted.iter().all(|alert| alert.generation == 2));
        state.services.get_mut("worker").unwrap().pid = None;
        assert!(alarms.evaluate(&mut state, &limits, None).is_empty());
        assert!(alarms.0.is_empty());
    }

    fn observe(
        alarms: &mut Alarms,
        state: &mut RuntimeSnapshot,
        limits: &BTreeMap<String, ResourceThresholds>,
        tick: i64,
        values: Option<(Option<f32>, u64)>,
    ) {
        let service = state.services.get_mut("worker").unwrap();
        service.resources = values.map(|(cpu_percent, memory_bytes)| ResourceUsage {
            cpu_percent,
            memory_bytes,
            sampled_at: service.started_at.unwrap() + chrono::Duration::seconds(tick),
        });
        alarms.evaluate(state, limits, None);
    }

    #[test]
    fn test_resource_restart_requires_three_distinct_samples_and_permission() {
        for action in [ResourceLimitAction::Warn, ResourceLimitAction::Restart] {
            let limits = [(
                "worker".into(),
                ResourceThresholds {
                    cpu_percent: Some(100),
                    memory_bytes: Some(100),
                    on_exceed: action,
                },
            )]
            .into();
            for cpu_trigger in [false, true] {
                let mut state = snapshot();
                let mut alarms = Alarms::default();
                let values = if cpu_trigger {
                    Some((Some(101.0), 50))
                } else {
                    Some((None, 101))
                };
                for tick in 1..=2 {
                    observe(&mut alarms, &mut state, &limits, tick, values);
                    // Duplicate snapshots cannot count as fresh samples.
                    for _ in 0..5 {
                        alarms.evaluate(&mut state, &limits, None);
                    }
                    assert!(state.services["worker"].resource_restart_reason.is_none());
                }
                observe(&mut alarms, &mut state, &limits, 3, values);
                assert_eq!(
                    state.services["worker"].resource_restart_reason.is_some(),
                    action == ResourceLimitAction::Restart
                );
                if action == ResourceLimitAction::Restart {
                    let reason = state.services["worker"]
                        .resource_restart_reason
                        .clone()
                        .unwrap();
                    assert!(reason.starts_with(if cpu_trigger { "CPU" } else { "RSS" }));
                    // Once decided, a healthy sample cannot erase the actor's request.
                    observe(&mut alarms, &mut state, &limits, 4, Some((Some(0.0), 50)));
                    assert_eq!(
                        state.services["worker"].resource_restart_reason.as_deref(),
                        Some(reason.as_str())
                    );
                }
            }
        }
    }

    #[test]
    fn test_resource_restart_streaks_reset_on_missing_normal_and_new_generation() {
        let limits = [(
            "worker".into(),
            ResourceThresholds {
                cpu_percent: Some(100),
                memory_bytes: Some(100),
                on_exceed: ResourceLimitAction::Restart,
            },
        )]
        .into();
        for interruption in [None, Some((None, 100)), Some((Some(100.0), 100))] {
            let mut state = snapshot();
            let mut alarms = Alarms::default();
            let high = Some((Some(150.0), 150));
            observe(&mut alarms, &mut state, &limits, 1, high);
            observe(&mut alarms, &mut state, &limits, 2, high);
            observe(&mut alarms, &mut state, &limits, 3, interruption);
            observe(&mut alarms, &mut state, &limits, 4, high);
            observe(&mut alarms, &mut state, &limits, 5, high);
            assert!(state.services["worker"].resource_restart_reason.is_none());
            state.services.get_mut("worker").unwrap().restart_count += 1;
            observe(&mut alarms, &mut state, &limits, 6, high);
            observe(&mut alarms, &mut state, &limits, 7, high);
            assert!(state.services["worker"].resource_restart_reason.is_none());
            observe(&mut alarms, &mut state, &limits, 8, high);
            assert!(state.services["worker"].resource_restart_reason.is_some());
        }
        // Alternating CPU/RSS spikes never add up to a sustained violation.
        let mut state = snapshot();
        let mut alarms = Alarms::default();
        for tick in 1..=10 {
            let values = if tick % 2 == 0 {
                (Some(150.0), 50)
            } else {
                (Some(50.0), 150)
            };
            observe(&mut alarms, &mut state, &limits, tick, Some(values));
            assert!(state.services["worker"].resource_restart_reason.is_none());
        }
    }

    #[test]
    fn test_resource_restart_rejects_late_samples_and_stopping_services() {
        let limits = [(
            "worker".into(),
            ResourceThresholds {
                cpu_percent: None,
                memory_bytes: Some(100),
                on_exceed: ResourceLimitAction::Restart,
            },
        )]
        .into();
        let mut state = snapshot();
        let mut alarms = Alarms::default();
        observe(&mut alarms, &mut state, &limits, 1, Some((None, 200)));
        observe(&mut alarms, &mut state, &limits, 2, Some((None, 200)));
        let old_generation = Generation::of(&state.services["worker"]).unwrap();
        let service = state.services.get_mut("worker").unwrap();
        service.restart_count += 1;
        service.resources = None;
        assert!(!apply(
            &mut state,
            [(
                "worker".into(),
                Sample {
                    generation: old_generation,
                    resources: Some(usage()),
                }
            )]
            .into()
        ));
        alarms.evaluate(&mut state, &limits, None);
        assert!(state.services["worker"].resource_restart_reason.is_none());
        state.services.get_mut("worker").unwrap().status = ServiceState::Stopping;
        for tick in 3..=6 {
            observe(&mut alarms, &mut state, &limits, tick, Some((None, 200)));
            assert!(state.services["worker"].resource_restart_reason.is_none());
        }
    }

    #[test]
    fn test_resources_reject_late_samples_after_exit_or_generation_change() {
        let original = snapshot();
        let generation = Generation::of(&original.services["worker"]).unwrap();
        for change in 0..4 {
            let mut current = original.clone();
            let state = current.services.get_mut("worker").unwrap();
            match change {
                0 => state.pid = None,
                1 => state.pid = Some(generation.pid + 1),
                2 => state.started_at = Some(generation.started_at + chrono::Duration::seconds(1)),
                _ => state.restart_count += 1,
            }
            let before = current.clone();
            assert!(!apply(
                &mut current,
                [(
                    "worker".into(),
                    Sample {
                        generation: generation.clone(),
                        resources: Some(usage()),
                    }
                )]
                .into()
            ));
            assert_eq!(current, before);
        }
    }

    #[test]
    fn test_resources_update_and_clear_without_changing_lifecycle() {
        let mut current = snapshot();
        let generation = Generation::of(&current.services["worker"]).unwrap();
        let before = current.clone();
        assert!(apply(
            &mut current,
            [(
                "worker".into(),
                Sample {
                    generation: generation.clone(),
                    resources: Some(usage()),
                }
            )]
            .into()
        ));
        assert!(apply(
            &mut current,
            [(
                "worker".into(),
                Sample {
                    generation,
                    resources: None,
                }
            )]
            .into()
        ));
        assert_eq!(current, before);
    }

    #[test]
    fn test_resources_old_snapshot_without_metrics_remains_readable() {
        let mut json = serde_json::to_value(snapshot()).unwrap();
        json["services"]["worker"]
            .as_object_mut()
            .unwrap()
            .remove("resources");
        let old: RuntimeSnapshot = serde_json::from_value(json).unwrap();
        assert!(old.services["worker"].resources.is_none());
    }

    #[test]
    fn test_resources_sampler_warms_up_and_retires_generation_caches() {
        let mut current = snapshot();
        let mut sampler = Sampler::default();
        let first = sampler.sample(&current);
        let first = first["worker"]
            .resources
            .as_ref()
            .expect("current process must be observable");
        assert!(first.memory_bytes > 0);
        assert!(first.cpu_percent.is_none());
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(250)));
        let second = sampler.sample(&current);
        assert!(second["worker"]
            .resources
            .as_ref()
            .unwrap()
            .cpu_percent
            .is_some());
        // Same PID, different supervisor generation: never reuse CPU history.
        current.services.get_mut("worker").unwrap().restart_count += 1;
        let restarted = sampler.sample(&current);
        assert!(restarted["worker"]
            .resources
            .as_ref()
            .unwrap()
            .cpu_percent
            .is_none());
        assert_eq!(sampler.processes.len(), 1);
        current.services.get_mut("worker").unwrap().pid = None;
        assert!(sampler.sample(&current).is_empty());
        assert!(sampler.processes.is_empty());
    }
}
