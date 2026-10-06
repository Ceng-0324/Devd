//! Periodic resource samples for service leaders, independent of process control.

use std::{collections::BTreeMap, time::Duration};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::sync::watch;

use crate::{
    config::ResourceThresholds,
    logging::{LogCollector, LogLevel},
};

use super::service_manager::{RuntimeSnapshot, ServiceSnapshot};

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
}

impl Generation {
    fn of(state: &ServiceSnapshot) -> Option<Self> {
        Some(Self {
            pid: state.pid?,
            started_at: state.started_at?,
            restart_count: state.restart_count,
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
        snapshot: &RuntimeSnapshot,
        limits: &BTreeMap<String, ResourceThresholds>,
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
            let Some(state) = snapshot.services.get(name) else {
                continue;
            };
            let (Some(generation), Some(usage)) = (Generation::of(state), &state.resources) else {
                continue;
            };
            let alarm = self.0.entry(name.clone()).or_insert_with(|| Alarm {
                generation: generation.clone(),
                cpu: false,
                memory: false,
            });
            if let (Some(limit), Some(value)) = (thresholds.cpu_percent, usage.cpu_percent) {
                let exceeded = f64::from(value) > f64::from(limit);
                if exceeded != alarm.cpu {
                    alarm.cpu = exceeded;
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
            }
            if let Some(limit) = thresholds.memory_bytes {
                let exceeded = usage.memory_bytes > limit;
                if exceeded != alarm.memory {
                    alarm.memory = exceeded;
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
        }
        alerts
    }
}

pub(super) async fn run(
    snapshots: watch::Sender<RuntimeSnapshot>,
    logs: LogCollector,
    limits: BTreeMap<String, ResourceThresholds>,
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
                snapshots.send_if_modified(|snapshot| apply(snapshot, samples));
                let alerts = alarms.evaluate(&snapshots.borrow(), &limits);
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
            services: [(
                "worker".into(),
                ServiceSnapshot {
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
            },
        )]
        .into();
        let mut state = snapshot();
        state.services.get_mut("worker").unwrap().resources = Some(usage());
        let mut alarms = Alarms::default();
        let first = alarms.evaluate(&state, &limits);
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|alert| alert.level == LogLevel::Warn));
        assert!(alarms.evaluate(&state, &limits).is_empty());
        state.services.get_mut("worker").unwrap().resources = None;
        assert!(alarms.evaluate(&state, &limits).is_empty());
        state.services.get_mut("worker").unwrap().resources = Some(ResourceUsage {
            cpu_percent: None,
            ..usage()
        });
        assert!(alarms.evaluate(&state, &limits).is_empty());
        let service = state.services.get_mut("worker").unwrap();
        service.resources = Some(ResourceUsage {
            cpu_percent: Some(100.0),
            memory_bytes: 40_000,
            sampled_at: Utc::now(),
        });
        let recovered = alarms.evaluate(&state, &limits);
        assert_eq!(recovered.len(), 2);
        assert!(recovered.iter().all(|alert| alert.level == LogLevel::Info));
        let service = state.services.get_mut("worker").unwrap();
        service.restart_count += 1;
        service.resources = Some(usage());
        assert_eq!(alarms.evaluate(&state, &limits).len(), 2);
        state.services.get_mut("worker").unwrap().restart_count += 1;
        let restarted = alarms.evaluate(&state, &limits);
        assert_eq!(restarted.len(), 2);
        assert!(restarted.iter().all(|alert| alert.generation == 2));
        state.services.get_mut("worker").unwrap().pid = None;
        assert!(alarms.evaluate(&state, &limits).is_empty());
        assert!(alarms.0.is_empty());
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
