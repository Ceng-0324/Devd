//! Opt-in application log storage using the shared rotating JSONL store.
use super::{LogEntry, LogFilter, LogLevel, OutputSummary};
use crate::storage::{JsonlStorage, Record};
use std::{collections::VecDeque, io, path::Path, sync::Arc};
use tokio::sync::broadcast;
// Preserve the published v0.4 library import path for log storage options.
pub use crate::storage::StorageOptions;
// Includes JSON escaping of the collector's maximum 1 MiB message.
const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;

pub struct LogStorage {
    inner: JsonlStorage,
}
impl LogStorage {
    /// Open under the supervisor state lock, before spawning services.
    pub fn open(directory: &Path, options: StorageOptions) -> io::Result<Self> {
        let (inner, removed) = JsonlStorage::open(directory, options, MAX_RECORD_BYTES)?;
        let mut storage = Self { inner };
        if removed > 0 {
            storage.append(&diagnostic(format!("discarded {removed} bytes from an unfinished stored log record after an interrupted write")))?;
        }
        Ok(storage)
    }
    /// Drain complete entries without blocking service capture. A slow disk may
    /// lag the bounded broadcast; each gap is recorded explicitly in the file.
    /// Graceful shutdown waits for this drain and syncs the final file.
    pub fn run(
        mut self,
        mut entries: broadcast::Receiver<Arc<LogEntry>>,
    ) -> io::Result<OutputSummary> {
        let mut summary = OutputSummary::default();
        loop {
            match entries.blocking_recv() {
                Ok(entry) => {
                    self.append(&entry)?;
                    summary.entries = summary.entries.saturating_add(1);
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    summary.dropped = summary.dropped.saturating_add(count);
                    self.append(&diagnostic(format!(
                        "disk log storage skipped {count} entries: subscriber lagged"
                    )))?;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        self.inner.sync()?;
        Ok(summary)
    }

    fn append(&mut self, entry: &LogEntry) -> io::Result<()> {
        self.inner.append(entry)
    }
}

/// Read an offline snapshot oldest first, filtering before the bounded tail.
/// An unfinished final record is ignored; complete corrupt records are errors.
pub fn read_stored(
    directory: &Path,
    service: Option<&str>,
    filter: &LogFilter,
    limit: usize,
) -> io::Result<Vec<LogEntry>> {
    if !(1..=1000).contains(&limit) {
        return Err(invalid("stored log tail must be between 1 and 1000"));
    }
    let mut result = VecDeque::new();
    crate::storage::scan(directory, MAX_RECORD_BYTES, |record| {
        if let Record::Complete(line) = record {
            let entry: LogEntry = serde_json::from_slice(line)
                .map_err(|error| invalid(format!("invalid log record: {error}")))?;
            if service.is_none_or(|name| entry.service == name) && filter.matches(&entry) {
                if result.len() == limit {
                    result.pop_front();
                }
                result.push_back(entry);
            }
        }
        Ok(())
    })?;
    Ok(result.into_iter().collect())
}

fn diagnostic(message: String) -> LogEntry {
    LogEntry {
        timestamp: chrono::Utc::now(),
        service: "devd".into(),
        generation: 0,
        level: LogLevel::Warn,
        message,
        truncated: false,
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{open_file, record_path as log_path};
    #[cfg(unix)]
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::{fs, io::Write};

    fn entry(index: usize) -> LogEntry {
        LogEntry {
            timestamp: "2026-10-06T00:00:00Z".parse().unwrap(),
            service: "worker".into(),
            generation: 0,
            level: LogLevel::Info,
            message: format!("line-{index:03}"),
            truncated: false,
        }
    }

    fn read(directory: &Path, limit: usize) -> Vec<LogEntry> {
        read_stored(directory, None, &LogFilter::default(), limit).unwrap()
    }

    #[test]
    fn test_storage_rotation_retention_and_append_across_runs() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let max_file_bytes = (serde_json::to_vec(&entry(0)).unwrap().len() as u64 + 1) * 2;
        let options = StorageOptions {
            max_file_bytes,
            keep: 2,
        };
        let mut storage = LogStorage::open(&path, options).unwrap();
        for index in 0..9 {
            storage.append(&entry(index)).unwrap();
        }
        drop(storage);
        assert_eq!(read(&path, 100), (4..9).map(entry).collect::<Vec<_>>());
        for index in 0..=2 {
            assert!(fs::metadata(log_path(&path, index)).unwrap().len() <= max_file_bytes);
        }
        let mut storage = LogStorage::open(&path, options).unwrap();
        storage.append(&entry(9)).unwrap();
        drop(storage);
        assert_eq!(read(&path, 3), (7..10).map(entry).collect::<Vec<_>>());
        let storage = LogStorage::open(&path, StorageOptions { keep: 1, ..options }).unwrap();
        drop(storage);
        assert_eq!(read(&path, 100), (6..10).map(entry).collect::<Vec<_>>());
        assert!(!log_path(&path, 2).exists());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(log_path(&path, 0))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn test_storage_filter_before_tail_and_escape_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let mut storage = LogStorage::open(&path, StorageOptions::default()).unwrap();
        let mut expected = entry(1);
        expected.message = "database\n\t\r\u{1b}中文".into();
        expected.generation = 9;
        expected.level = LogLevel::Error;
        expected.truncated = true;
        storage.append(&expected).unwrap();
        storage.append(&entry(2)).unwrap();
        let mut other = expected.clone();
        other.service = "other".into();
        storage.append(&other).unwrap();
        drop(storage);
        let filter = LogFilter {
            level: Some(LogLevel::Error),
            since: Some(expected.timestamp),
            grep: Some("database".into()),
        };
        assert_eq!(
            read_stored(&path, Some("worker"), &filter, 1).unwrap(),
            [expected]
        );
        assert_eq!(
            fs::read_to_string(log_path(&path, 0))
                .unwrap()
                .lines()
                .count(),
            3
        );
        assert!(read_stored(&path, Some("missing"), &filter, 1)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_storage_recovers_only_unfinished_tail_and_reports_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let mut storage = LogStorage::open(&path, StorageOptions::default()).unwrap();
        storage.append(&entry(0)).unwrap();
        drop(storage);
        open_file(&log_path(&path, 0), true)
            .unwrap()
            .write_all(b"{\"message\":")
            .unwrap();
        assert_eq!(read(&path, 10), [entry(0)]);
        let mut storage = LogStorage::open(&path, StorageOptions::default()).unwrap();
        storage.append(&entry(1)).unwrap();
        drop(storage);
        let entries = read(&path, 10);
        assert_eq!(entries[0], entry(0));
        assert_eq!(entries[1].level, LogLevel::Warn);
        assert!(entries[1].message.contains("unfinished stored log record"));
        assert_eq!(entries[2], entry(1));
        open_file(&log_path(&path, 0), true)
            .unwrap()
            .write_all(b"broken\n")
            .unwrap();
        assert!(read_stored(&path, None, &LogFilter::default(), 10)
            .unwrap_err()
            .to_string()
            .contains("invalid log record"));
    }

    #[test]
    fn test_storage_lock_prevents_competing_writers_and_offline_reads() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let storage = LogStorage::open(&path, StorageOptions::default()).unwrap();
        assert!(LogStorage::open(&path, StorageOptions::default()).is_err());
        assert_eq!(
            read_stored(&path, None, &LogFilter::default(), 10)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(storage);
        assert!(read(&path, 10).is_empty());
    }

    #[test]
    fn test_storage_reads_rotation_gaps_and_rejects_incomplete_archives() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let mut storage = LogStorage::open(&path, StorageOptions::default()).unwrap();
        storage.append(&entry(2)).unwrap();
        drop(storage);
        let mut oldest = serde_json::to_vec(&entry(0)).unwrap();
        oldest.push(b'\n');
        fs::write(log_path(&path, 3), oldest).unwrap();
        assert_eq!(read(&path, 10), [entry(0), entry(2)]);
        fs::write(log_path(&path, 1), serde_json::to_vec(&entry(1)).unwrap()).unwrap();
        assert!(read_stored(&path, None, &LogFilter::default(), 10)
            .unwrap_err()
            .to_string()
            .contains("unfinished JSONL record"));
    }

    #[test]
    fn test_storage_rejects_symlinks_and_non_regular_paths_without_touching_targets() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("outside");
        fs::write(&target, "keep me").unwrap();
        #[cfg(unix)]
        for name in [".lock", "current.jsonl", "archive-1.jsonl"] {
            let sub = tempfile::tempdir().unwrap();
            symlink(&target, sub.path().join(name)).unwrap();
            assert!(LogStorage::open(sub.path(), StorageOptions::default()).is_err());
            assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
        }
        let path = root.path().join("logs");
        fs::create_dir(&path).unwrap();
        fs::create_dir(log_path(&path, 0)).unwrap();
        assert!(LogStorage::open(&path, StorageOptions::default()).is_err());
        #[cfg(unix)]
        {
            let link = root.path().join("linked");
            symlink(&path, &link).unwrap();
            assert!(LogStorage::open(&link, StorageOptions::default()).is_err());
        }
        let hardlink_dir = tempfile::tempdir().unwrap();
        fs::hard_link(&target, log_path(hardlink_dir.path(), 0)).unwrap();
        assert!(LogStorage::open(hardlink_dir.path(), StorageOptions::default()).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
    }

    #[test]
    fn test_storage_lag_is_explicit_and_shutdown_drains_records() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let storage = LogStorage::open(&path, StorageOptions::default()).unwrap();
        let (sender, receiver) = broadcast::channel(2);
        for index in 0..5 {
            sender.send(Arc::new(entry(index))).unwrap();
        }
        drop(sender);
        let summary = storage.run(receiver).unwrap();
        assert_eq!(
            summary,
            OutputSummary {
                entries: 2,
                dropped: 3
            }
        );
        let entries = read(&path, 10);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].message.contains("skipped 3 entries"));
        assert_eq!(entries[0].level, LogLevel::Warn);
        assert_eq!(&entries[1..], &[entry(3), entry(4)]);
    }

    #[test]
    fn test_storage_enforces_record_options_and_query_bounds() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        for options in [
            StorageOptions {
                max_file_bytes: 0,
                keep: 1,
            },
            StorageOptions {
                max_file_bytes: u64::MAX,
                keep: 1,
            },
            StorageOptions {
                max_file_bytes: 1024,
                keep: 0,
            },
            StorageOptions {
                max_file_bytes: 1024,
                keep: 101,
            },
        ] {
            assert!(LogStorage::open(&path, options).is_err());
            assert!(!path.exists());
        }
        let mut storage = LogStorage::open(
            &path,
            StorageOptions {
                max_file_bytes: 1,
                keep: 1,
            },
        )
        .unwrap();
        assert!(storage.append(&entry(0)).is_err());
        drop(storage);
        assert!(read(&path, 10).is_empty());
        assert!(read_stored(&path, None, &LogFilter::default(), 0).is_err());
        assert!(read_stored(&path, None, &LogFilter::default(), 1001).is_err());
        fs::write(log_path(&path, 0), vec![b'x'; MAX_RECORD_BYTES + 1]).unwrap();
        assert!(read_stored(&path, None, &LogFilter::default(), 1)
            .unwrap_err()
            .to_string()
            .contains("oversized"));
        assert!(LogStorage::open(&path, StorageOptions::default()).is_err());
    }
}
