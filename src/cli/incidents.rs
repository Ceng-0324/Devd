//! Opt-in evidence collection. Never sends service controls or drains application output.
pub(super) mod evidence;
pub(super) mod query;
mod relationships;
mod storage;

use super::{export::SafeServiceState, instances::Identity};
use crate::{
    config::DevdConfig,
    core::{
        events::{EventData, LifecycleEvent},
        service_manager::{RuntimeSnapshot, ServiceState},
    },
};
use anyhow::{Context, Result};
use chrono::Utc;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc, watch, OwnedSemaphorePermit, Semaphore},
    time::Instant,
};

const WINDOW: Duration = Duration::from_secs(5);
const MAX_PENDING: usize = 4;
const EVENT_LIMIT: usize = 200;
const EVENT_BYTES: usize = 256 * 1024;

pub(super) use storage::Storage;

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct Statistics {
    admitted: u64,
    merged: u64,
    dropped_triggers: u64,
    skipped_events: u64,
}
impl Statistics {
    pub(super) fn notice(&self) -> Option<String> {
        (self.dropped_triggers > 0 || self.skipped_events > 0).then(|| {
            format!(
                "incident coverage incomplete: {} triggers dropped, {} events skipped\n",
                self.dropped_triggers, self.skipped_events
            )
        })
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(super) enum EndReason {
    Deadline,
    GenerationReplaced,
    Reload,
    Stopping,
    Stopped,
    Cancelled,
    Disconnected,
}

struct Pending {
    id: String,
    service: String,
    generation: Option<u64>,
    deadline: Instant,
    finished: Arc<AtomicBool>,
    following: Tail,
    merged: u64,
    skipped_at_start: u64,
    _permit: Arc<OwnedSemaphorePermit>,
}

/// Both record count and serialized bytes are bounded independently.
#[derive(Default)]
struct Tail {
    entries: VecDeque<(LifecycleEvent, usize)>,
    bytes: usize,
    omitted: u64,
}
impl Tail {
    fn push(&mut self, mut event: LifecycleEvent) {
        super::export::sanitize_event(&mut event);
        let size = serde_json::to_vec(&event)
            .expect("typed lifecycle event")
            .len();
        while self.entries.len() >= EVENT_LIMIT || self.bytes + size > EVENT_BYTES {
            let Some((_, old)) = self.entries.pop_front() else {
                self.omitted += 1;
                return;
            };
            self.bytes -= old;
            self.omitted += 1;
        }
        self.bytes += size;
        self.entries.push_back((event, size));
    }
    fn events(&self) -> Vec<LifecycleEvent> {
        self.entries.iter().map(|(e, _)| e.clone()).collect()
    }
}

struct Cancel(Arc<AtomicBool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub(super) struct Source {
    pub identity: Identity,
    pub events: broadcast::Receiver<Arc<LifecycleEvent>>,
    pub snapshots: watch::Receiver<RuntimeSnapshot>,
    pub configurations: watch::Receiver<Arc<DevdConfig>>,
}

/// One async subscriber and one blocking worker, with at most four admitted
/// captures including queued/actively written work. No per-trigger task spawning.
pub(super) async fn run(storage: Storage, source: Source) -> Result<Statistics> {
    let mut events = source.events;
    let mut prior = Tail::default();
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel = Cancel(cancelled.clone());
    let (send, receive) = mpsc::channel(MAX_PENDING * 2);
    let mut worker = tokio::task::spawn_blocking(move || storage.run(receive, cancelled));
    let permits = Arc::new(Semaphore::new(MAX_PENDING));
    let mut pending: Vec<Pending> = vec![];
    let mut statistics = Statistics::default();
    let mut stopping = false;
    let mut controlled = BTreeMap::new();
    loop {
        let deadline = pending
            .iter()
            .map(|p| p.deadline)
            .min()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(86400));
        tokio::select! {
            biased;
            result = &mut worker => { result.context("incident worker failed")??; return Ok(statistics); },
            _ = tokio::time::sleep_until(deadline), if !pending.is_empty() => {
                finish_matching(&mut pending, &send, &statistics, EndReason::Deadline, |p| p.deadline <= Instant::now())?;
            },
            received = events.recv() => match received {
                Err(broadcast::error::RecvError::Closed) => {
                    finish_matching(&mut pending, &send, &statistics, EndReason::Disconnected, |_| true)?;
                    break;
                },
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    statistics.skipped_events = statistics.skipped_events.saturating_add(count);
                },
                Ok(event) => {
                    for capture in &mut pending { capture.following.push((*event).clone()); }
                    let boundary = match &event.data {
                        EventData::GenerationPending => Some(EndReason::GenerationReplaced),
                        EventData::ReloadStarted { .. } => Some(EndReason::Reload),
                        EventData::SupervisorStopping { .. } => { stopping = true; Some(EndReason::Stopping) },
                        EventData::SupervisorStopped { .. } => { stopping = true; Some(EndReason::Stopped) },
                        EventData::SupervisorCancelled => { stopping = true; Some(EndReason::Cancelled) },
                        _ => None,
                    };
                    if let Some(reason) = boundary {
                        finish_matching(&mut pending, &send, &statistics, reason, |p| reason != EndReason::GenerationReplaced || event.service.as_deref() == Some(&p.service))?;
                    }
                    if let Some(name) = &event.service {
                        match event.data {
                            EventData::GenerationPending => { controlled.remove(name); },
                            EventData::ServiceStopRequested { .. } | EventData::StateChanged { to: ServiceState::Stopping, .. } => {
                                if controlled.len() >= 256 && !controlled.contains_key(name) { anyhow::bail!("incident control context exceeds 256 services"); }
                                controlled.insert(name.clone(), event.generation);
                            },
                            _ => {},
                        }
                    }
                    let intentional_exit = stopping || event.service.as_ref().is_some_and(|name| controlled.get(name) == Some(&event.generation));
                    if eligible(&event.data, intentional_exit) {
                        if let Some(name) = &event.service {
                            if let Some(capture) = pending.iter_mut().find(|p| p.service == *name && p.generation == event.generation) {
                                capture.merged += 1;
                                statistics.merged += 1;
                            } else if let Ok(permit) = permits.clone().try_acquire_owned() {
                                let id = uuid::Uuid::new_v4().to_string();
                                let permit = Arc::new(permit);
                                let finished = Arc::new(AtomicBool::new(false));
                                let config = source.configurations.borrow().clone();
                                let observed_at = Utc::now();
                                // These are observations when consumed, not an atomic
                                // snapshot of the instant the trigger was emitted.
                                let snapshot = source.snapshots.borrow();
                                let state = snapshot.services.get(name).map(SafeServiceState::from);
                                let observed_generation = snapshot.services.get(name).and_then(|s| s.event_generation);
                                drop(snapshot);
                                statistics.admitted = statistics.admitted.saturating_add(1);
                                let capture = evidence::Begin {
                                    schema_version: 2, id: id.clone(), identity: source.identity.clone(),
                                    trigger: (*event).clone(), observed_at, observed_generation, state,
                                    prior: prior.events(), prior_omitted: prior.omitted,
                                    statistics: statistics.clone(),
                                };
                                send.try_send(storage::Work::Begin {
                                    capture: Box::new(capture), config, permit: permit.clone(),
                                    finished: finished.clone(), deadline: std::time::Instant::now() + WINDOW,
                                }).context("incident queue unavailable")?;
                                pending.push(Pending { id, service: name.clone(), generation: event.generation,
                                    deadline: Instant::now() + WINDOW, finished, following: Tail::default(), merged: 0,
                                    skipped_at_start: statistics.skipped_events, _permit: permit });
                            } else { statistics.dropped_triggers += 1; }
                        }
                    }
                    prior.push((*event).clone());
                }
            }
        }
    }
    drop(send);
    worker.await.context("incident worker failed")??;
    Ok(statistics)
}

fn finish_matching(
    pending: &mut Vec<Pending>,
    send: &mpsc::Sender<storage::Work>,
    statistics: &Statistics,
    reason: EndReason,
    matches: impl Fn(&Pending) -> bool,
) -> Result<()> {
    let mut index = 0;
    while index < pending.len() {
        if !matches(&pending[index]) {
            index += 1;
            continue;
        }
        let capture = pending.remove(index);
        capture.finished.store(true, Ordering::Release);
        send.try_send(storage::Work::End(evidence::End {
            id: capture.id,
            reason,
            observed_at: Utc::now(),
            following: capture.following.events(),
            following_omitted: Some(capture.following.omitted),
            merged_triggers: Some(capture.merged),
            skipped_events: Some(
                statistics
                    .skipped_events
                    .saturating_sub(capture.skipped_at_start),
            ),
            statistics: Some(statistics.clone()),
        }))
        .context("incident completion queue unavailable")?;
    }
    Ok(())
}

fn eligible(data: &EventData, intentional_exit: bool) -> bool {
    match data {
        EventData::Exited { code, signal } => {
            !intentional_exit && (signal.is_some() || code.is_some_and(|c| c != 0))
        }
        EventData::TaskTimedOut { .. }
        | EventData::SpawnFailed { .. }
        | EventData::ProcessFailed { .. }
        | EventData::ActorFailed { .. }
        | EventData::DependencyFailed { .. }
        | EventData::DependencyTimedOut { .. } => true,
        EventData::StateChanged {
            to: ServiceState::Failed,
            ..
        } => true,
        EventData::HealthChanged {
            state: ServiceState::Unhealthy,
            ..
        } => true,
        EventData::ResourceChanged { exceeded: true, .. } => true,
        EventData::PathConditionChanged { evidence } => evidence.failure.is_some(),
        _ => false,
    }
}

#[cfg(test)]
mod tests;
