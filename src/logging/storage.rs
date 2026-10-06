//! Bounded JSONL storage. Call blocking operations on Tokio's blocking pool.
//! A lifetime lock keeps offline reads and rotations mutually exclusive.

use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::platform::files;
use tokio::sync::broadcast;

use super::{LogEntry, LogFilter, LogLevel, OutputSummary};

const MAX_ARCHIVES: u16 = 100;
// Includes JSON escaping of the collector's maximum 1 MiB message.
const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct StorageOptions {
    pub max_file_bytes: u64,
    /// Rotated archives in addition to the current file.
    pub keep: u16,
}

impl Default for StorageOptions {
    fn default() -> Self {
        Self {
            max_file_bytes: 10 * 1024 * 1024,
            keep: 3,
        }
    }
}

pub struct LogStorage {
    directory: PathBuf,
    options: StorageOptions,
    file: File,
    size: u64,
    _lock: File,
}

impl LogStorage {
    /// Open only after acquiring the supervisor's state lock, before spawning
    /// services. Existing complete records survive across supervisor runs.
    pub fn open(directory: &Path, options: StorageOptions) -> io::Result<Self> {
        if options.max_file_bytes == 0 || options.max_file_bytes > 1024 * 1024 * 1024 {
            return Err(invalid("log file size must be between 1 byte and 1 GiB"));
        }
        if !(1..=MAX_ARCHIVES).contains(&options.keep) {
            return Err(invalid("log archive count must be between 1 and 100"));
        }
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        match builder.create(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        check_directory(directory)?;
        let lock = lock(directory, true)?;
        // Check every managed path before repairing or rotating any file.
        for index in 0..=MAX_ARCHIVES {
            check_file(&log_path(directory, index))?;
        }
        let mut file = open_file(&log_path(directory, 0), true)?;
        let removed = repair_tail(&mut file)?;
        let size = file.metadata()?.len();
        let mut storage = Self {
            directory: directory.into(),
            options,
            file,
            size,
            _lock: lock,
        };
        // A reduced retention setting also applies to archives from older runs.
        for index in options.keep + 1..=MAX_ARCHIVES {
            remove_if_present(&log_path(directory, index))?;
        }
        if storage.size > options.max_file_bytes {
            storage.rotate()?;
        }
        if removed > 0 {
            storage.append(&diagnostic(format!(
                "discarded {removed} bytes from an unfinished stored log record after an interrupted write"
            )))?;
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
        self.file.sync_data()?;
        Ok(summary)
    }

    fn append(&mut self, entry: &LogEntry) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(entry).map_err(io::Error::other)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_RECORD_BYTES || bytes.len() as u64 > self.options.max_file_bytes {
            return Err(invalid("stored log record exceeds the log file size limit"));
        }
        if self.size + bytes.len() as u64 > self.options.max_file_bytes {
            self.rotate()?;
        }
        self.file.write_all(&bytes)?;
        self.size += bytes.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.sync_data()?;
        for index in (0..self.options.keep).rev() {
            let source = log_path(&self.directory, index);
            let destination = log_path(&self.directory, index + 1);
            if check_file(&source)? {
                check_file(&destination)?;
                fs::rename(source, destination)?;
            }
        }
        self.file = open_file(&log_path(&self.directory, 0), true)?;
        self.size = 0;
        Ok(())
    }
}

/// Read an offline snapshot oldest first, filtering before applying the tail.
/// The YAML need not exist. Memory is bounded by `limit` and record size, even
/// for corrupt files. An unfinished last record of the current file is ignored.
pub fn read_stored(
    directory: &Path,
    service: Option<&str>,
    filter: &LogFilter,
    limit: usize,
) -> io::Result<Vec<LogEntry>> {
    if !(1..=1000).contains(&limit) {
        return Err(invalid("stored log tail must be between 1 and 1000"));
    }
    check_directory(directory)?;
    let _lock = lock(directory, false)?;
    let mut result = VecDeque::new();
    for index in (0..=MAX_ARCHIVES).rev() {
        let path = log_path(directory, index);
        if !check_file(&path)? {
            continue;
        }
        let mut reader = BufReader::new(open_file(&path, false)?);
        let mut line = Vec::new();
        loop {
            line.clear();
            Read::by_ref(&mut reader)
                .take(MAX_RECORD_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)?;
            if line.is_empty() {
                break;
            }
            if line.len() > MAX_RECORD_BYTES {
                return Err(invalid(format!(
                    "oversized log record in {}",
                    path.display()
                )));
            }
            if line.last() != Some(&b'\n') {
                if index == 0 {
                    break;
                }
                return Err(invalid(format!(
                    "unfinished log record in {}",
                    path.display()
                )));
            }
            let entry: LogEntry = serde_json::from_slice(&line).map_err(|error| {
                invalid(format!("invalid log record in {}: {error}", path.display()))
            })?;
            if service.is_none_or(|name| entry.service == name) && filter.matches(&entry) {
                if result.len() == limit {
                    result.pop_front();
                }
                result.push_back(entry);
            }
        }
    }
    Ok(result.into_iter().collect())
}

fn log_path(directory: &Path, index: u16) -> PathBuf {
    directory.join(if index == 0 {
        "current.jsonl".into()
    } else {
        format!("archive-{index}.jsonl")
    })
}

fn check_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || files::is_link(&metadata) {
        return Err(invalid(format!("not a log directory: {}", path.display())));
    }
    Ok(())
}

fn check_file(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if files::regular(&metadata) => {
            files::open_regular(path, false, true)?;
            Ok(true)
        }
        Ok(_) => Err(invalid(format!(
            "not a regular log file: {}",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn open_file(path: &Path, write: bool) -> io::Result<File> {
    let mut file = files::open_regular(path, write, true)?;
    if write {
        file.seek(SeekFrom::End(0))?;
    }
    Ok(file)
}

fn lock(directory: &Path, write: bool) -> io::Result<File> {
    let file = open_file(&directory.join(".lock"), write)?;
    let result = if write {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    result.map_err(|error| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("stored logs are in use; stop the supervisor or use live 'logs': {error}"),
        )
    })?;
    Ok(file)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn repair_tail(file: &mut File) -> io::Result<u64> {
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(0);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(0);
    }
    let scan = length.min(MAX_RECORD_BYTES as u64);
    file.seek(SeekFrom::End(-(scan as i64)))?;
    let mut tail = Vec::with_capacity(scan as usize);
    file.take(scan).read_to_end(&mut tail)?;
    let removed = match tail.iter().rposition(|byte| *byte == b'\n') {
        Some(index) => scan - index as u64 - 1,
        None if scan == length => scan,
        None => {
            return Err(invalid(
                "unfinished stored log record exceeds the size limit",
            ))
        }
    };
    if removed > 0 {
        file.set_len(length - removed)?;
        // Truncation preserves the old cursor. On Windows writes use that
        // position; continuing there would insert a zero-filled gap.
        file.seek(SeekFrom::End(0))?;
    }
    Ok(removed)
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
    #[cfg(unix)]
    use std::os::unix::fs::{symlink, PermissionsExt};

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
            .contains("unfinished log record"));
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
