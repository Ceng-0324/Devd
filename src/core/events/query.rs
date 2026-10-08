//! Query metadata makes retention and subscription gaps observable.
use std::str::FromStr;

use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use super::{EventContext, EventData, EventWindow, LifecycleEvent, EVENT_SCHEMA_VERSION};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    SupervisorStarted,
    SupervisorStopping,
    SupervisorStopped,
    SupervisorCancelled,
    GenerationPending,
    Starting,
    Started,
    Exited,
    StateChanged,
    HealthChanged,
    DependencyWaiting,
    DependencyReady,
    DependencyFailed,
    DependencyTimedOut,
    SpawnFailed,
    ProcessFailed,
    ActorFailed,
    ManualRestartRequested,
    ReloadStarted,
    ReloadServiceSelected,
    ReloadFinished,
    ServiceStopRequested,
    RestartTriggered,
    RestartDecision,
    ResourceChanged,
    ResourceRestartRequested,
    PathConditionChanged,
    Omitted,
}

impl EventData {
    pub fn kind(&self) -> EventKind {
        match self {
            Self::SupervisorStarted => EventKind::SupervisorStarted,
            Self::SupervisorStopping { .. } => EventKind::SupervisorStopping,
            Self::SupervisorStopped { .. } => EventKind::SupervisorStopped,
            Self::SupervisorCancelled => EventKind::SupervisorCancelled,
            Self::GenerationPending => EventKind::GenerationPending,
            Self::Starting => EventKind::Starting,
            Self::Started { .. } => EventKind::Started,
            Self::Exited { .. } => EventKind::Exited,
            Self::StateChanged { .. } => EventKind::StateChanged,
            Self::HealthChanged { .. } => EventKind::HealthChanged,
            Self::DependencyWaiting { .. } => EventKind::DependencyWaiting,
            Self::DependencyReady { .. } => EventKind::DependencyReady,
            Self::DependencyFailed { .. } => EventKind::DependencyFailed,
            Self::DependencyTimedOut { .. } => EventKind::DependencyTimedOut,
            Self::SpawnFailed { .. } => EventKind::SpawnFailed,
            Self::ProcessFailed { .. } => EventKind::ProcessFailed,
            Self::ActorFailed { .. } => EventKind::ActorFailed,
            Self::ManualRestartRequested => EventKind::ManualRestartRequested,
            Self::ReloadStarted { .. } => EventKind::ReloadStarted,
            Self::ReloadServiceSelected { .. } => EventKind::ReloadServiceSelected,
            Self::ReloadFinished { .. } => EventKind::ReloadFinished,
            Self::ServiceStopRequested { .. } => EventKind::ServiceStopRequested,
            Self::RestartTriggered { .. } => EventKind::RestartTriggered,
            Self::RestartDecision { .. } => EventKind::RestartDecision,
            Self::ResourceChanged { .. } => EventKind::ResourceChanged,
            Self::ResourceRestartRequested { .. } => EventKind::ResourceRestartRequested,
            Self::PathConditionChanged { .. } => EventKind::PathConditionChanged,
            Self::Omitted { .. } => EventKind::Omitted,
        }
    }
}

/// The next sequence to inspect, inclusive; run identity prevents PID reuse
/// and a sequence reset from silently changing what a cursor addresses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EventCursor {
    pub run_id: String,
    pub next_sequence: u64,
}

impl std::fmt::Display for EventCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.run_id, self.next_sequence)
    }
}

impl FromStr for EventCursor {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (run, sequence) = text
            .split_once(':')
            .ok_or("cursor must be RUN_UUID:NEXT_SEQUENCE")?;
        let uuid = uuid::Uuid::parse_str(run).map_err(|_| "invalid cursor run UUID")?;
        let next_sequence = sequence.parse().map_err(|_| "invalid cursor sequence")?;
        Ok(Self {
            run_id: uuid.to_string(),
            next_sequence,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventFilter {
    pub service: Option<String>,
    #[serde(default)]
    pub kinds: Vec<EventKind>,
    pub since: Option<DateTime<Utc>>,
}

impl EventFilter {
    pub fn matches(&self, event: &LifecycleEvent) -> bool {
        self.service
            .as_ref()
            .is_none_or(|name| event.service.as_ref() == Some(name))
            && (self.kinds.is_empty() || self.kinds.contains(&event.data.kind()))
            && self.since.is_none_or(|since| event.timestamp >= since)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventQuery {
    pub filter: EventFilter,
    pub tail: usize,
    pub cursor: Option<EventCursor>,
}

impl EventQuery {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=1000).contains(&self.tail) {
            return Err("event tail must be between 1 and 1000".into());
        }
        if self.filter.kinds.len() > EventKind::value_variants().len() {
            return Err("too many event type selectors".into());
        }
        if let Some(cursor) = &self.cursor {
            uuid::Uuid::parse_str(&cursor.run_id).map_err(|_| "invalid cursor run UUID")?;
        }
        Ok(())
    }

    pub fn select(&self, window: EventWindow) -> Result<EventBatch, String> {
        self.validate()?;
        let mut gaps = Vec::new();
        let mut start = 0;
        if let Some(cursor) = &self.cursor {
            if cursor.run_id != window.context.run_id {
                gaps.push(EventGap::DifferentRun {
                    requested: cursor.clone(),
                });
            } else if cursor.next_sequence > window.next_sequence {
                return Err("cursor is ahead of this run's next sequence".into());
            } else {
                start = cursor.next_sequence;
            }
        }
        if start < window.first_available {
            gaps.push(EventGap::Retention {
                from: start,
                to: window.first_available,
            });
        }
        let matches: Vec<_> = window
            .entries
            .into_iter()
            .filter(|event| event.sequence >= start && self.filter.matches(event))
            .collect();
        let skipped = matches.len().saturating_sub(self.tail);
        if skipped > 0 {
            gaps.push(EventGap::TailLimited {
                matching_records: skipped as u64,
            });
        }
        Ok(EventBatch {
            schema_version: EVENT_SCHEMA_VERSION,
            source: EventSource::Live,
            persistence: None,
            omitted_gaps: 0,
            cursor: Some(EventCursor {
                run_id: window.context.run_id.clone(),
                next_sequence: window.next_sequence,
            }),
            context: Some(window.context),
            first_available: Some(window.first_available),
            gaps,
            entries: matches
                .into_iter()
                .skip(skipped)
                .map(|event| (*event).clone())
                .collect(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "reason", rename_all = "kebab-case")]
pub enum EventGap {
    Retention { from: u64, to: u64 },
    DifferentRun { requested: EventCursor },
    TailLimited { matching_records: u64 },
    SubscriberLag { run_id: String, skipped: u64 },
    UnfinishedRecord { bytes: u64 },
    StoredSequence { run_id: String, from: u64, to: u64 },
    StoredCursorMissing { requested: EventCursor },
    RecoveredTail { bytes: u64 },
    IncompleteRun { run_id: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EventSource {
    Live,
    Stored,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PersistenceState {
    Disabled,
    Recording,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventBatch {
    #[serde(deserialize_with = "super::read_schema_version")]
    pub schema_version: u16,
    pub source: EventSource,
    /// Live writer status; offline files cannot certify the current process.
    pub persistence: Option<PersistenceState>,
    pub context: Option<EventContext>,
    pub first_available: Option<u64>,
    pub cursor: Option<EventCursor>,
    pub gaps: Vec<EventGap>,
    /// Older gap diagnostics discarded to keep offline queries bounded.
    pub omitted_gaps: u64,
    pub entries: Vec<LifecycleEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::{EventRecorder, EVENT_CAPACITY};

    fn query() -> EventQuery {
        EventQuery {
            filter: EventFilter::default(),
            tail: 1000,
            cursor: None,
        }
    }

    #[test]
    fn test_event_query_filters_before_tail_and_reports_watermark() {
        let recorder = EventRecorder::new("state".into(), None);
        recorder.record(Some("api"), None, None, EventData::Starting);
        recorder.record(Some("other"), None, None, EventData::Starting);
        recorder.record(Some("api"), None, None, EventData::Started { pid: None });
        recorder.record(Some("api"), None, None, EventData::Starting);
        let mut query = query();
        query.tail = 1;
        query.filter.service = Some("api".into());
        query.filter.kinds = vec![EventKind::Starting];
        let batch = query.select(recorder.history().snapshot()).unwrap();
        assert_eq!(batch.entries[0].sequence, 3);
        assert_eq!(batch.cursor.unwrap().next_sequence, 4);
        assert_eq!(
            batch.gaps,
            [EventGap::TailLimited {
                matching_records: 1
            }]
        );
        query.filter.since = Some(Utc::now() + chrono::Duration::hours(1));
        let batch = query.select(recorder.history().snapshot()).unwrap();
        assert!(batch.entries.is_empty());
        assert_eq!(batch.cursor.unwrap().next_sequence, 4);
    }

    #[test]
    fn test_event_cursors_detect_retention_run_changes_and_invalid_input() {
        let recorder = EventRecorder::new("state".into(), None);
        for _ in 0..EVENT_CAPACITY + 3 {
            recorder.record(None, None, None, EventData::Starting);
        }
        let mut query = query();
        query.cursor = Some(EventCursor {
            run_id: recorder.run_id().into(),
            next_sequence: 1,
        });
        assert!(query
            .select(recorder.history().snapshot())
            .unwrap()
            .gaps
            .contains(&EventGap::Retention { from: 1, to: 3 }));
        query.cursor.as_mut().unwrap().next_sequence = 2000;
        assert!(query.select(recorder.history().snapshot()).is_err());
        query.cursor.as_mut().unwrap().run_id = uuid::Uuid::new_v4().to_string();
        assert!(matches!(
            query.select(recorder.history().snapshot()).unwrap().gaps[0],
            EventGap::DifferentRun { .. }
        ));
        let cursor = format!("{}:1027", recorder.run_id());
        assert_eq!(cursor.parse::<EventCursor>().unwrap().to_string(), cursor);
        for input in ["none", "abc:1", "00000000-0000-0000-0000-000000000000:-1"] {
            assert!(input.parse::<EventCursor>().is_err());
        }
        query.tail = 0;
        assert!(query.validate().is_err());
        query.tail = 1001;
        assert!(query.validate().is_err());
    }

    #[test]
    fn test_event_history_subscription_is_atomic_and_does_not_keep_producer_alive() {
        let recorder = EventRecorder::new("state".into(), None);
        let producer = recorder.clone();
        let handle = std::thread::spawn(move || {
            for _ in 0..100 {
                producer.record(None, None, None, EventData::Starting);
                std::thread::yield_now();
            }
        });
        let history = recorder.history();
        let (window, mut live) = history.subscribe_with_snapshot();
        handle.join().unwrap();
        drop(recorder);
        let mut sequences: Vec<_> = window.entries.iter().map(|e| e.sequence).collect();
        while let Ok(event) = live.try_recv() {
            sequences.push(event.sequence);
        }
        assert_eq!(sequences, (0..100).collect::<Vec<_>>());
        assert!(matches!(
            live.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Closed)
        ));
        let (window, mut ended) = history.subscribe_with_snapshot();
        assert_eq!(window.next_sequence, 100);
        assert!(matches!(
            ended.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Closed)
        ));
    }
}
