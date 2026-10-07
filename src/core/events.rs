//! Bounded lifecycle facts. This module never owns processes or drives recovery.
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::{sync::broadcast, time::Instant};

use super::{
    health_check::ProbeFailure, process_manager::ProcessError, service_manager::ServiceState,
};
use crate::config::{BackoffType, DependencyCondition, RestartPolicy, RestartPolicyType};

pub const EVENT_SCHEMA_VERSION: u16 = 1;
pub const EVENT_CAPACITY: usize = 1024;
pub const MAX_EVENT_BYTES: usize = 16 * 1024;
const LIVE_CAPACITY: usize = 256;

pub mod query;
pub mod storage;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventContext {
    pub run_id: String,
    /// Same instance/profile-specific state path used by the manager.
    pub state_path: PathBuf,
    pub profile: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LifecycleEvent {
    #[serde(deserialize_with = "read_schema_version")]
    pub schema_version: u16,
    pub run_id: String,
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    /// Monotonic elapsed time since this recorder was created, not wall time.
    pub elapsed: Duration,
    pub service: Option<String>,
    /// Sequence of GenerationPending, independent of PID and retry counters.
    pub generation: Option<u64>,
    /// Earlier event in this run that directly caused this observation/action.
    pub cause: Option<u64>,
    pub truncated: bool,
    pub data: EventData,
}

fn read_schema_version<'de, D: serde::Deserializer<'de>>(de: D) -> Result<u16, D::Error> {
    let version = u16::deserialize(de)?;
    if version == EVENT_SCHEMA_VERSION {
        Ok(version)
    } else {
        Err(serde::de::Error::custom(
            "unsupported lifecycle event schema version",
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum EventData {
    SupervisorStarted,
    SupervisorStopping {
        reason: ShutdownReason,
    },
    SupervisorStopped {
        failed: bool,
    },
    SupervisorCancelled,
    GenerationPending,
    Starting,
    Started {
        pid: Option<u32>,
    },
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    StateChanged {
        from: ServiceState,
        to: ServiceState,
    },
    HealthChanged {
        state: ServiceState,
        consecutive_failures: u32,
        failure: Option<ProbeEvidence>,
    },
    DependencyWaiting {
        service: String,
        condition: DependencyCondition,
        observed_generation: Option<u64>,
        timeout: Duration,
        remaining: Duration,
    },
    DependencyReady {
        service: String,
        condition: DependencyCondition,
        observed_generation: Option<u64>,
    },
    DependencyFailed {
        service: String,
        state: ServiceState,
    },
    DependencyTimedOut {
        timeout: Duration,
    },
    SpawnFailed {
        failure: ProcessEvidence,
    },
    ProcessFailed {
        failure: ProcessEvidence,
    },
    ActorFailed {
        cancelled: bool,
    },
    ManualRestartRequested,
    ServiceStopRequested {
        manual_restart: bool,
    },
    RestartTriggered {
        reason: RestartCause,
        policy: RestartSettings,
    },
    RestartDecision {
        outcome: RestartOutcome,
        policy: RestartSettings,
        restart_count: u32,
        delay: Option<Duration>,
    },
    ResourceChanged {
        exceeded: bool,
        evidence: ResourceEvidence,
    },
    ResourceRestartRequested {
        evidence: ResourceEvidence,
    },
    /// Payload exceeded the record bound; no unbounded text is retained.
    Omitted {
        original_type: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ShutdownReason {
    Requested,
    ServiceFailed,
    StateWriteFailed,
    ActorFailed,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RestartOutcome {
    Scheduled,
    PolicyDeclined,
    BudgetExhausted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum RestartCause {
    ProcessExit,
    SpawnFailure,
    HealthFailure,
    DependencyRecovery { services: Vec<DependencyChange> },
    ResourceLimit { evidence: Option<ResourceEvidence> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DependencyChange {
    pub service: String,
    pub previous_generation: Option<u64>,
    pub current_generation: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RestartSettings {
    pub policy: RestartPolicyType,
    pub backoff: BackoffType,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub max_attempts: u32,
}
impl From<&RestartPolicy> for RestartSettings {
    fn from(p: &RestartPolicy) -> Self {
        Self {
            policy: p.policy.clone(),
            backoff: p.backoff.clone(),
            initial_delay: p.initial_delay,
            max_delay: p.max_delay,
            max_attempts: p.max_attempts,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "metric", rename_all = "kebab-case")]
pub enum ResourceValue {
    Cpu { percent: f32, limit_percent: u32 },
    Memory { bytes: u64, limit_bytes: u64 },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResourceEvidence {
    pub value: ResourceValue,
    pub sampled_at: DateTime<Utc>,
    pub consecutive_samples: u8,
    pub restart_authorized: bool,
}

/// Only typed failure facts: URLs, commands and environment-file contents are
/// deliberately absent, including when upstream errors embed those values.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ProbeEvidence {
    Timeout { timeout: Duration },
    Tcp { os_code: Option<i32> },
    Socket { os_code: Option<i32> },
    Http { connection_error: bool },
    HttpStatus { status: u16 },
    Script,
}
impl From<&ProbeFailure> for ProbeEvidence {
    fn from(f: &ProbeFailure) -> Self {
        match f {
            ProbeFailure::Timeout { timeout } => Self::Timeout { timeout: *timeout },
            ProbeFailure::Tcp { source } => Self::Tcp {
                os_code: source.raw_os_error(),
            },
            ProbeFailure::Socket { source } => Self::Socket {
                os_code: source.raw_os_error(),
            },
            ProbeFailure::Http { source } => Self::Http {
                connection_error: source.is_connect(),
            },
            ProbeFailure::HttpStatus { status } => Self::HttpStatus {
                status: status.as_u16(),
            },
            ProbeFailure::Script { .. } => Self::Script,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "operation", rename_all = "kebab-case")]
pub enum ProcessEvidence {
    Command,
    EnvironmentRead { os_code: Option<i32> },
    EnvironmentParse,
    Spawn { os_code: Option<i32> },
    Wait { os_code: Option<i32> },
    Cleanup,
    InvalidGracePeriod,
    Signal { os_code: i32 },
    Job { os_code: Option<i32> },
}
impl From<&ProcessError> for ProcessEvidence {
    fn from(e: &ProcessError) -> Self {
        match e {
            ProcessError::EmptyCommand { .. } | ProcessError::CommandParse { .. } => Self::Command,
            ProcessError::EnvironmentRead { source, .. } => Self::EnvironmentRead {
                os_code: source.raw_os_error(),
            },
            ProcessError::EnvironmentParse { .. } => Self::EnvironmentParse,
            ProcessError::Spawn { source, .. } => Self::Spawn {
                os_code: source.raw_os_error(),
            },
            ProcessError::Wait { source, .. } => Self::Wait {
                os_code: source.raw_os_error(),
            },
            ProcessError::InvalidGracePeriod { .. } => Self::InvalidGracePeriod,
            #[cfg(unix)]
            ProcessError::CleanupTimeout { .. } => Self::Cleanup,
            #[cfg(unix)]
            ProcessError::Signal { source, .. } => Self::Signal {
                os_code: *source as i32,
            },
            #[cfg(windows)]
            ProcessError::JobCleanupTimeout { .. } => Self::Cleanup,
            #[cfg(windows)]
            ProcessError::Job { source, .. } => Self::Job {
                os_code: source.raw_os_error(),
            },
        }
    }
}

struct Buffer {
    next: u64,
    entries: VecDeque<Arc<LifecycleEvent>>,
}

/// A consistent history window. An earlier sequence than first_available was
/// evicted, not evidence that the event never happened. No snapshot is implied.
#[derive(Debug, Clone)]
pub struct EventWindow {
    pub context: EventContext,
    pub first_available: u64,
    pub next_sequence: u64,
    pub entries: Vec<Arc<LifecycleEvent>>,
}

/// Read-only handle; retaining history does not keep the live channel open.
#[derive(Clone)]
pub struct EventHistory {
    context: Arc<EventContext>,
    buffer: Arc<Mutex<Buffer>>,
    sender: broadcast::WeakSender<Arc<LifecycleEvent>>,
}
impl EventHistory {
    pub fn snapshot(&self) -> EventWindow {
        let buffer = self.buffer.lock().unwrap();
        EventWindow {
            context: (*self.context).clone(),
            first_available: buffer.entries.front().map_or(buffer.next, |e| e.sequence),
            next_sequence: buffer.next,
            entries: buffer.entries.iter().cloned().collect(),
        }
    }

    /// Capture a history window and subscribe under the publisher lock. The
    /// first live event is at or beyond the window's next_sequence.
    pub fn subscribe_with_snapshot(
        &self,
    ) -> (EventWindow, broadcast::Receiver<Arc<LifecycleEvent>>) {
        let buffer = self.buffer.lock().unwrap();
        let live = self
            .sender
            .upgrade()
            .map_or_else(|| broadcast::channel(1).1, |sender| sender.subscribe());
        let window = EventWindow {
            context: (*self.context).clone(),
            first_available: buffer.entries.front().map_or(buffer.next, |e| e.sequence),
            next_sequence: buffer.next,
            entries: buffer.entries.iter().cloned().collect(),
        };
        (window, live)
    }
}

#[derive(Clone)]
pub(crate) struct EventRecorder {
    history: EventHistory,
    sender: broadcast::Sender<Arc<LifecycleEvent>>,
    started: Instant,
}
impl EventRecorder {
    pub fn new(state_path: PathBuf, profile: Option<String>) -> Self {
        let (sender, _) = broadcast::channel(LIVE_CAPACITY);
        Self {
            history: EventHistory {
                context: Arc::new(EventContext {
                    run_id: uuid::Uuid::new_v4().to_string(),
                    state_path,
                    profile,
                }),
                buffer: Arc::new(Mutex::new(Buffer {
                    next: 0,
                    entries: VecDeque::new(),
                })),
                sender: sender.downgrade(),
            },
            sender,
            started: Instant::now(),
        }
    }
    pub fn run_id(&self) -> &str {
        &self.history.context.run_id
    }

    pub fn history(&self) -> EventHistory {
        self.history.clone()
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<LifecycleEvent>> {
        self.sender.subscribe()
    }
    pub fn record(
        &self,
        service: Option<&str>,
        generation: Option<u64>,
        cause: Option<u64>,
        data: EventData,
    ) -> u64 {
        // Allocation, history insertion and broadcast share one short lock, so
        // concurrent producers cannot publish sequence N+1 before N.
        let mut buffer = self.history.buffer.lock().unwrap();
        let sequence = buffer.next;
        let generation = if matches!(data, EventData::GenerationPending) {
            Some(sequence)
        } else {
            generation
        };
        let mut event = LifecycleEvent {
            schema_version: EVENT_SCHEMA_VERSION,
            run_id: self.history.context.run_id.clone(),
            sequence,
            timestamp: Utc::now(),
            elapsed: self.started.elapsed(),
            service: service.map(str::to_owned),
            generation,
            cause,
            truncated: false,
            data,
        };
        if let Some(name) = &mut event.service {
            if name.len() > 256 {
                let mut end = 256;
                while !name.is_char_boundary(end) {
                    end -= 1;
                }
                name.truncate(end);
                event.truncated = true;
            }
        }
        let mut bound = RecordSize(0);
        if serde_json::to_writer(&mut bound, &event).is_err() {
            let original_type = match &event.data {
                EventData::DependencyWaiting { .. } => "dependency-waiting",
                EventData::DependencyReady { .. } => "dependency-ready",
                EventData::DependencyFailed { .. } => "dependency-failed",
                EventData::RestartTriggered { .. } => "restart-triggered",
                _ => "unknown",
            }
            .to_owned();
            event.data = EventData::Omitted { original_type };
            event.truncated = true;
        }
        let event = Arc::new(event);
        buffer.next = sequence
            .checked_add(1)
            .expect("lifecycle sequence exhausted");
        if buffer.entries.len() == EVENT_CAPACITY {
            buffer.entries.pop_front();
        }
        buffer.entries.push_back(event.clone());
        let _ = self.sender.send(event);
        sequence
    }
}

// Stop counting at the bound instead of allocating serialized copies of large
// user-provided identifiers or dependency lists.
struct RecordSize(usize);
impl std::io::Write for RecordSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_EVENT_BYTES.saturating_sub(self.0) {
            return Err(std::io::Error::other("event exceeds record size bound"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorder() -> EventRecorder {
        EventRecorder::new("state".into(), Some("dev".into()))
    }

    #[test]
    fn test_events_concurrent_order_matches_history_and_live_delivery() {
        let recorder = recorder();
        let mut live = recorder.subscribe();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let recorder = &recorder;
                scope.spawn(move || {
                    for _ in 0..16 {
                        recorder.record(None, None, None, EventData::SupervisorStarted);
                    }
                });
            }
        });
        let window = recorder.history().snapshot();
        for (i, event) in window.entries.iter().enumerate() {
            assert_eq!(event.sequence, i as u64);
            assert_eq!(*live.try_recv().unwrap(), **event);
            assert_eq!(event.run_id, window.context.run_id);
        }
    }

    #[test]
    fn test_events_eviction_lag_and_channel_close_are_explicit() {
        let recorder = recorder();
        let history = recorder.history();
        let mut live = recorder.subscribe();
        for _ in 0..EVENT_CAPACITY + 7 {
            recorder.record(None, None, None, EventData::SupervisorStarted);
        }
        let window = history.snapshot();
        assert_eq!(window.entries.len(), EVENT_CAPACITY);
        assert_eq!(window.first_available, 7);
        assert_eq!(window.next_sequence, (EVENT_CAPACITY + 7) as u64);
        assert!(matches!(
            live.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        drop(recorder);
        while live.try_recv().is_ok() {}
        assert!(matches!(
            live.try_recv(),
            Err(broadcast::error::TryRecvError::Closed)
        ));
        assert_eq!(history.snapshot().entries.len(), EVENT_CAPACITY);
    }

    #[test]
    fn test_events_bound_payloads_and_validate_schema() {
        let recorder = recorder();
        recorder.record(
            Some(&"猫".repeat(400)),
            None,
            None,
            EventData::DependencyWaiting {
                service: "x".repeat(MAX_EVENT_BYTES * 2),
                condition: DependencyCondition::Started,
                observed_generation: None,
                timeout: Duration::from_secs(30),
                remaining: Duration::from_secs(30),
            },
        );
        let event = recorder.history().snapshot().entries.remove(0);
        let encoded = serde_json::to_vec(&*event).unwrap();
        assert!(encoded.len() <= MAX_EVENT_BYTES);
        assert!(event.truncated);
        assert!(matches!(event.data, EventData::Omitted { .. }));
        assert_eq!(
            serde_json::from_slice::<LifecycleEvent>(&encoded).unwrap(),
            *event
        );
        let mut json = serde_json::to_value(&*event).unwrap();
        json["schema_version"] = 2.into();
        assert!(serde_json::from_value::<LifecycleEvent>(json).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn test_events_run_identity_generation_and_monotonic_elapsed() {
        let first = recorder();
        let second = recorder();
        assert_ne!(first.history.context.run_id, second.history.context.run_id);
        let generation = first.record(Some("api"), None, None, EventData::GenerationPending);
        tokio::time::advance(Duration::from_secs(2)).await;
        first.record(Some("api"), Some(generation), None, EventData::Starting);
        let events = first.history().snapshot().entries;
        assert_eq!(events[0].generation, Some(generation));
        assert_eq!(events[1].generation, Some(generation));
        assert_eq!(
            events[1].elapsed - events[0].elapsed,
            Duration::from_secs(2)
        );
    }

    #[test]
    fn test_event_failures_do_not_copy_commands_or_environment_contents() {
        let marker = "private-value-fixture";
        let errors = [
            ProcessError::Spawn {
                service: "api".into(),
                command: marker.into(),
                cwd: Some(marker.into()),
                source: std::io::Error::other(marker),
            },
            ProcessError::EnvironmentParse {
                service: "api".into(),
                path: marker.into(),
                source: dotenvy::Error::LineParse(marker.into(), 1),
            },
            ProcessError::EnvironmentRead {
                service: "api".into(),
                path: marker.into(),
                source: std::io::Error::other(marker),
            },
        ];
        for error in errors {
            assert!(error.to_string().contains(marker));
            let data = EventData::SpawnFailed {
                failure: (&error).into(),
            };
            assert!(!serde_json::to_string(&data).unwrap().contains(marker));
        }
        let error = ProbeFailure::Script {
            message: marker.into(),
        };
        assert!(!serde_json::to_string(&ProbeEvidence::from(&error))
            .unwrap()
            .contains(marker));
    }
}
