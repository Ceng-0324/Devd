//! Opt-in disk history, separate from application logs.
use std::{collections::VecDeque, io, path::Path, sync::Arc};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use super::{
    query::{EventBatch, EventCursor, EventGap, EventQuery, EventSource},
    EventContext, LifecycleEvent, EVENT_SCHEMA_VERSION,
};
use crate::storage::{JsonlStorage, Record, StorageOptions};

// Includes bounded event data plus context and framing. Reads allocate at most
// this amount per record, regardless of the size of a damaged input file.
const MAX_RECORD_BYTES: usize = 64 * 1024;
const MAX_GAPS: usize = 128;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRecord {
    #[serde(deserialize_with = "super::read_schema_version")]
    schema_version: u16,
    context: EventContext,
    record: StoredData,
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "record",
    content = "data",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
enum StoredData {
    Begin,
    Event(Box<LifecycleEvent>),
    Gap(EventGap),
    End { next_sequence: u64 },
}

pub struct EventStorage {
    inner: JsonlStorage,
    context: EventContext,
    next: u64,
}

impl EventStorage {
    pub fn open(
        directory: &Path,
        options: StorageOptions,
        context: EventContext,
    ) -> io::Result<Self> {
        let (inner, removed) = JsonlStorage::open(directory, options, MAX_RECORD_BYTES)?;
        let mut storage = Self {
            inner,
            context,
            next: 0,
        };
        if removed > 0 {
            storage.append(StoredData::Gap(EventGap::RecoveredTail { bytes: removed }))?;
        }
        storage.append(StoredData::Begin)?;
        Ok(storage)
    }

    fn append(&mut self, record: StoredData) -> io::Result<()> {
        self.inner.append(&StoredRecord {
            schema_version: EVENT_SCHEMA_VERSION,
            context: self.context.clone(),
            record,
        })
    }

    pub fn run(mut self, mut events: broadcast::Receiver<Arc<LifecycleEvent>>) -> io::Result<()> {
        loop {
            match events.blocking_recv() {
                Ok(event) => {
                    self.next = event.sequence + 1;
                    self.append(StoredData::Event(Box::new((*event).clone())))?;
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    self.next = self.next.saturating_add(skipped);
                    self.append(StoredData::Gap(EventGap::SubscriberLag {
                        run_id: self.context.run_id.clone(),
                        skipped,
                    }))?;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        self.append(StoredData::End {
            next_sequence: self.next,
        })?;
        self.inner.sync()
    }
}

/// Scan retained records in order, without loading the YAML or treating old
/// PIDs as live. Filtering, cursor selection, and tail never hide gap metadata.
pub fn read_stored(directory: &Path, query: &EventQuery) -> io::Result<EventBatch> {
    query.validate().map_err(invalid)?;
    let mut entries = VecDeque::new();
    let mut gaps = VecDeque::new();
    let mut omitted_gaps = 0u64;
    let mut gap = |item| {
        if gaps.len() == MAX_GAPS {
            gaps.pop_front();
            omitted_gaps += 1;
        }
        gaps.push_back(item);
    };
    let mut context: Option<EventContext> = None;
    let mut next = 0;
    let mut first_available = None;
    let mut ended = false;
    let mut began = false;
    let mut cursor_found = query.cursor.is_none();
    let mut cursor_run_seen = false;
    let mut skipped = 0u64;
    crate::storage::scan(directory, MAX_RECORD_BYTES, |record| {
        let Record::Complete(line) = record else {
            if let Record::Unfinished(bytes) = record {
                gap(EventGap::UnfinishedRecord { bytes });
            }
            return Ok(());
        };
        let record: StoredRecord = serde_json::from_slice(line)
            .map_err(|error| invalid(format!("invalid stored event record: {error}")))?;
        uuid::Uuid::parse_str(&record.context.run_id)
            .map_err(|_| invalid("invalid stored run UUID"))?;
        if context
            .as_ref()
            .is_none_or(|old| old.run_id != record.context.run_id)
        {
            if let Some(old) = &context {
                if !ended {
                    gap(EventGap::IncompleteRun {
                        run_id: old.run_id.clone(),
                    });
                }
                if cursor_run_seen && !cursor_found {
                    return Err(invalid("cursor is ahead of the stored run"));
                }
            }
            context = Some(record.context.clone());
            next = 0;
            first_available = None;
            ended = false;
            began = false;
        } else if context.as_ref() != Some(&record.context) {
            return Err(invalid("stored run context changed within a run"));
        }
        let in_cursor_run = query
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.run_id == record.context.run_id);
        cursor_run_seen |= in_cursor_run;
        match record.record {
            StoredData::Event(event) => {
                if ended || event.run_id != record.context.run_id || event.sequence < next {
                    return Err(invalid(
                        "stored events are out of order or have a mismatched run",
                    ));
                }
                first_available.get_or_insert(event.sequence);
                if event.sequence > next {
                    gap(EventGap::StoredSequence {
                        run_id: event.run_id.clone(),
                        from: next,
                        to: event.sequence,
                    });
                }
                next = event
                    .sequence
                    .checked_add(1)
                    .ok_or_else(|| invalid("stored sequence overflow"))?;
                if in_cursor_run && event.sequence >= query.cursor.as_ref().unwrap().next_sequence {
                    cursor_found = true;
                }
                if cursor_found && query.filter.matches(&event) {
                    if entries.len() == query.tail {
                        entries.pop_front();
                        skipped += 1;
                    }
                    entries.push_back(*event);
                }
            }
            StoredData::Gap(item) => {
                if ended {
                    return Err(invalid("stored gap follows run ending"));
                }
                if let EventGap::SubscriberLag { run_id, .. } = &item {
                    if run_id != &record.context.run_id {
                        return Err(invalid("stored gap has a mismatched run"));
                    }
                }
                gap(item);
            }
            StoredData::Begin => {
                if began || next != 0 || ended {
                    return Err(invalid("misplaced stored run beginning"));
                }
                began = true;
            }
            StoredData::End { next_sequence } => {
                if ended || next_sequence < next {
                    return Err(invalid("invalid stored run ending"));
                }
                if next_sequence > next {
                    gap(EventGap::StoredSequence {
                        run_id: record.context.run_id,
                        from: next,
                        to: next_sequence,
                    });
                }
                next = next_sequence;
                ended = true;
            }
        }
        // A cursor at the retained run's end is valid even with no matches.
        if in_cursor_run && query.cursor.as_ref().unwrap().next_sequence <= next {
            cursor_found = true;
        }
        Ok(())
    })?;
    if let Some(context) = &context {
        if !ended {
            gap(EventGap::IncompleteRun {
                run_id: context.run_id.clone(),
            });
        }
    }
    if cursor_run_seen && !cursor_found {
        return Err(invalid("cursor is ahead of the stored run"));
    }
    if !cursor_found {
        gap(EventGap::StoredCursorMissing {
            requested: query.cursor.clone().unwrap(),
        });
    }
    if skipped > 0 {
        gap(EventGap::TailLimited {
            matching_records: skipped,
        });
    }
    Ok(EventBatch {
        schema_version: EVENT_SCHEMA_VERSION,
        source: EventSource::Stored,
        persistence: None,
        cursor: context.as_ref().map(|context| EventCursor {
            run_id: context.run_id.clone(),
            next_sequence: next,
        }),
        context,
        first_available,
        gaps: gaps.into_iter().collect(),
        omitted_gaps,
        entries: entries.into_iter().collect(),
    })
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::{
        query::{EventFilter, EventKind},
        EventData, EventRecorder,
    };
    use std::{fs, io::Write};

    fn query() -> EventQuery {
        EventQuery {
            filter: EventFilter::default(),
            tail: 1000,
            cursor: None,
        }
    }

    fn save(path: &Path, options: StorageOptions, count: usize) -> String {
        let recorder = EventRecorder::new("state".into(), Some("test".into()));
        let run = recorder.run_id().to_owned();
        let storage =
            EventStorage::open(path, options, recorder.history().snapshot().context).unwrap();
        let receiver = recorder.subscribe();
        for _ in 0..count {
            recorder.record(Some("api"), None, None, EventData::Starting);
        }
        drop(recorder);
        storage.run(receiver).unwrap();
        run
    }

    #[test]
    fn test_stored_events_runs_cursors_filtering_and_drain() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("events");
        let first = save(&path, StorageOptions::default(), 4);
        let second = save(&path, StorageOptions::default(), 3);
        let all = read_stored(&path, &query()).unwrap();
        assert!(all.gaps.is_empty());
        assert_eq!(all.entries.len(), 7);
        assert_eq!(all.cursor.unwrap().run_id, second);
        let mut q = query();
        q.cursor = Some(EventCursor {
            run_id: first,
            next_sequence: 3,
        });
        let batch = read_stored(&path, &q).unwrap();
        assert_eq!(batch.entries.len(), 4);
        assert_eq!(batch.entries[0].sequence, 3);
        q.cursor.as_mut().unwrap().next_sequence = 5;
        assert!(read_stored(&path, &q)
            .unwrap_err()
            .to_string()
            .contains("ahead"));
        q.cursor = Some(EventCursor {
            run_id: second,
            next_sequence: 3,
        });
        assert!(read_stored(&path, &q).unwrap().entries.is_empty());
        q.cursor = None;
        q.filter.kinds = vec![EventKind::Exited];
        assert!(read_stored(&path, &q).unwrap().entries.is_empty());
        q.filter.kinds.clear();
        q.tail = 2;
        let batch = read_stored(&path, &q).unwrap();
        assert_eq!(batch.entries.len(), 2);
        assert_eq!(
            batch.gaps,
            [EventGap::TailLimited {
                matching_records: 5
            }]
        );
        q.cursor = Some(EventCursor {
            run_id: uuid::Uuid::new_v4().to_string(),
            next_sequence: 0,
        });
        let batch = read_stored(&path, &q).unwrap();
        assert!(batch.entries.is_empty());
        assert!(matches!(
            batch.gaps[0],
            EventGap::StoredCursorMissing { .. }
        ));
    }

    #[test]
    fn test_stored_events_rotation_retention_lag_and_locks() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("events");
        save(
            &path,
            StorageOptions {
                max_file_bytes: 1500,
                keep: 2,
            },
            12,
        );
        let batch = read_stored(&path, &query()).unwrap();
        assert!(batch.entries.len() < 12);
        assert!(matches!(
            batch.gaps[0],
            EventGap::StoredSequence { from: 0, .. }
        ));
        for index in 0..=2 {
            assert!(
                fs::metadata(crate::storage::record_path(&path, index))
                    .unwrap()
                    .len()
                    <= 1500
            );
        }
        let recorder = EventRecorder::new("state".into(), None);
        let storage = EventStorage::open(
            &path,
            StorageOptions::default(),
            recorder.history().snapshot().context,
        )
        .unwrap();
        assert!(EventStorage::open(
            &path,
            StorageOptions::default(),
            recorder.history().snapshot().context
        )
        .is_err());
        assert_eq!(
            read_stored(&path, &query()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let receiver = recorder.subscribe();
        for _ in 0..300 {
            recorder.record(None, None, None, EventData::Starting);
        }
        drop(recorder);
        storage.run(receiver).unwrap();
        let batch = read_stored(&path, &query()).unwrap();
        assert!(batch
            .gaps
            .iter()
            .any(|g| matches!(g, EventGap::SubscriberLag { skipped: 44, .. })));
        assert!(!batch
            .gaps
            .iter()
            .any(|g| matches!(g, EventGap::IncompleteRun { .. })));
    }

    #[test]
    fn test_stored_events_partial_tail_recovery_corruption_schema_and_gap_bounds() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("events");
        let recorder = EventRecorder::new("state".into(), None);
        let storage = EventStorage::open(
            &path,
            StorageOptions::default(),
            recorder.history().snapshot().context,
        )
        .unwrap();
        drop(storage);
        let file = path.join("current.jsonl");
        crate::storage::open_file(&file, true)
            .unwrap()
            .write_all(b"{partial")
            .unwrap();
        let batch = read_stored(&path, &query()).unwrap();
        assert!(batch
            .gaps
            .contains(&EventGap::UnfinishedRecord { bytes: 8 }));
        assert!(batch
            .gaps
            .iter()
            .any(|g| matches!(g, EventGap::IncompleteRun { .. })));
        save(&path, StorageOptions::default(), 1);
        assert!(read_stored(&path, &query())
            .unwrap()
            .gaps
            .contains(&EventGap::RecoveredTail { bytes: 8 }));
        let content = fs::read_to_string(&file).unwrap();
        fs::write(
            &file,
            content.replacen("\"schema_version\":1", "\"schema_version\":999", 1),
        )
        .unwrap();
        assert!(read_stored(&path, &query())
            .unwrap_err()
            .to_string()
            .contains("unsupported"));
        fs::write(&file, "broken\n").unwrap();
        assert!(read_stored(&path, &query())
            .unwrap_err()
            .to_string()
            .contains("invalid stored event"));
        fs::write(&file, vec![b'x'; MAX_RECORD_BYTES + 1]).unwrap();
        assert!(read_stored(&path, &query())
            .unwrap_err()
            .to_string()
            .contains("oversized"));
        fs::write(&file, "").unwrap();
        let mut storage = EventStorage::open(
            &path,
            StorageOptions::default(),
            recorder.history().snapshot().context,
        )
        .unwrap();
        for _ in 0..200 {
            storage
                .append(StoredData::Gap(EventGap::SubscriberLag {
                    run_id: recorder.run_id().into(),
                    skipped: 1,
                }))
                .unwrap();
        }
        drop(storage);
        let batch = read_stored(&path, &query()).unwrap();
        assert_eq!(batch.gaps.len(), MAX_GAPS);
        assert_eq!(batch.omitted_gaps, 73);
    }

    #[test]
    fn test_stored_events_reject_duplicate_beginnings_and_records_after_end() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("events");
        let recorder = EventRecorder::new("state".into(), None);
        let mut storage = EventStorage::open(
            &path,
            StorageOptions::default(),
            recorder.history().snapshot().context,
        )
        .unwrap();
        storage.append(StoredData::Begin).unwrap();
        drop(storage);
        assert!(read_stored(&path, &query())
            .unwrap_err()
            .to_string()
            .contains("misplaced"));
        fs::write(path.join("current.jsonl"), "").unwrap();
        let mut storage = EventStorage::open(
            &path,
            StorageOptions::default(),
            recorder.history().snapshot().context,
        )
        .unwrap();
        storage
            .append(StoredData::End { next_sequence: 0 })
            .unwrap();
        storage
            .append(StoredData::Gap(EventGap::SubscriberLag {
                run_id: recorder.run_id().into(),
                skipped: 1,
            }))
            .unwrap();
        drop(storage);
        assert!(read_stored(&path, &query())
            .unwrap_err()
            .to_string()
            .contains("follows run ending"));
    }

    #[test]
    fn test_event_writer_failure_leaves_an_observable_incomplete_run() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("events");
        let recorder = EventRecorder::new("state".into(), None);
        let mut storage = EventStorage::open(
            &path,
            StorageOptions {
                max_file_bytes: 1500,
                keep: 1,
            },
            recorder.history().snapshot().context,
        )
        .unwrap();
        fs::create_dir(path.join("archive-1.jsonl")).unwrap();
        let mut failed = false;
        for _ in 0..10 {
            if storage
                .append(StoredData::Gap(EventGap::SubscriberLag {
                    run_id: recorder.run_id().into(),
                    skipped: 1,
                }))
                .is_err()
            {
                failed = true;
                break;
            }
        }
        assert!(failed);
        drop(storage);
        fs::remove_dir(path.join("archive-1.jsonl")).unwrap();
        assert!(read_stored(&path, &query())
            .unwrap()
            .gaps
            .iter()
            .any(|g| matches!(g, EventGap::IncompleteRun { .. })));
    }
}
