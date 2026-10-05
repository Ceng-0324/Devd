use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex},
};

use chrono::Utc;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::broadcast,
};

use super::{LogEntry, LogLevel};

#[derive(Debug, Clone, Copy)]
pub struct LogOptions {
    /// Total retained entries across all services, from 1 to 65,536.
    pub capacity: usize,
    /// Retained input bytes per line, from 1 to 1 MiB; excess is discarded.
    pub max_line_bytes: usize,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            capacity: 1000,
            max_line_bytes: 16 * 1024,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LogOptionsError {
    #[error("log capacity must be between 1 and 65536 entries")]
    InvalidCapacity,
    #[error("max log line size must be between 1 and 1048576 bytes")]
    InvalidLineSize,
}

/// Read-only bounded history. This handle does not keep live subscriptions open.
#[derive(Clone)]
pub struct LogHistory {
    entries: Arc<Mutex<VecDeque<Arc<LogEntry>>>>,
    sender: broadcast::WeakSender<Arc<LogEntry>>,
}

impl LogHistory {
    /// Return up to `limit` newest matching entries, ordered oldest first.
    pub fn recent(&self, service: Option<&str>, limit: usize) -> Vec<Arc<LogEntry>> {
        let store = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Self::filtered(&store, service, limit)
    }

    /// Subscribe and snapshot under the same lock used by publishers.
    /// After collection ends, return retained history and a closed receiver.
    pub fn subscribe_with_recent(
        &self,
        service: Option<&str>,
        limit: usize,
    ) -> (Vec<Arc<LogEntry>>, broadcast::Receiver<Arc<LogEntry>>) {
        let store = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let receiver = match self.sender.upgrade() {
            Some(sender) => sender.subscribe(),
            None => broadcast::channel(1).1,
        };
        (Self::filtered(&store, service, limit), receiver)
    }

    fn filtered(
        store: &VecDeque<Arc<LogEntry>>,
        service: Option<&str>,
        limit: usize,
    ) -> Vec<Arc<LogEntry>> {
        let mut entries: Vec<_> = store
            .iter()
            .rev()
            .filter(|entry| service.is_none_or(|service| entry.service == service))
            .take(limit)
            .cloned()
            .collect();
        entries.reverse();
        entries
    }
}

/// Capture complete lines before broadcasting, so lag never corrupts framing.
/// Publishing and history insertion share a short lock to establish one order.
#[derive(Clone)]
pub struct LogCollector {
    options: LogOptions,
    history: LogHistory,
    sender: broadcast::Sender<Arc<LogEntry>>,
}

impl LogCollector {
    pub fn new(options: LogOptions) -> Result<Self, LogOptionsError> {
        if !(1..=65_536).contains(&options.capacity) {
            return Err(LogOptionsError::InvalidCapacity);
        }
        if !(1..=1024 * 1024).contains(&options.max_line_bytes) {
            return Err(LogOptionsError::InvalidLineSize);
        }
        let (sender, _) = broadcast::channel(256);
        let history = LogHistory {
            entries: Arc::new(Mutex::new(VecDeque::new())),
            sender: sender.downgrade(),
        };
        Ok(Self {
            options,
            history,
            sender,
        })
    }

    pub fn history(&self) -> LogHistory {
        self.history.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<LogEntry>> {
        self.sender.subscribe()
    }

    /// Frame one process stream. stdout callers use Info, stderr callers Error.
    /// EOF, I/O failure, and cancellation flush any unfinished tail exactly once.
    /// Invalid UTF-8 is decoded lossily after framing, not per read chunk.
    pub async fn collect(
        &self,
        mut reader: impl AsyncRead + Unpin,
        service: String,
        generation: u32,
        level: LogLevel,
    ) -> io::Result<()> {
        let mut stream = StreamCollector {
            collector: self.clone(),
            service,
            generation,
            level,
            bytes: Vec::new(),
            truncated: false,
            extra_cr: false,
        };
        let mut buffer = [0; 8192];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) => return Ok(()),
                Ok(size) => {
                    stream.push(&buffer[..size]);
                    tokio::task::yield_now().await;
                }
                Err(error) => {
                    stream.finish();
                    self.record(
                        &stream.service,
                        generation,
                        LogLevel::Warn,
                        format!("{level:?} stream read failed: {error}"),
                        false,
                    );
                    return Err(error);
                }
            }
        }
    }

    fn record(
        &self,
        service: &str,
        generation: u32,
        level: LogLevel,
        message: String,
        truncated: bool,
    ) {
        let mut store = self
            .history
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let entry = Arc::new(LogEntry {
            timestamp: Utc::now(),
            service: service.into(),
            generation,
            level,
            message,
            truncated,
        });
        if store.len() == self.options.capacity {
            store.pop_front();
        }
        store.push_back(entry.clone());
        let _ = self.sender.send(entry);
    }
}

struct StreamCollector {
    collector: LogCollector,
    service: String,
    generation: u32,
    level: LogLevel,
    bytes: Vec<u8>,
    truncated: bool,
    extra_cr: bool,
}

impl StreamCollector {
    fn push(&mut self, input: &[u8]) {
        for &byte in input {
            if byte == b'\n' {
                if !self.truncated && !self.extra_cr && self.bytes.last() == Some(&b'\r') {
                    self.bytes.pop();
                }
                self.extra_cr = false;
                self.emit();
            } else if !self.truncated {
                if self.extra_cr {
                    self.extra_cr = false;
                    self.truncated = true;
                } else if self.bytes.len() < self.collector.options.max_line_bytes {
                    self.bytes.push(byte);
                } else if byte == b'\r' {
                    // CRLF is a delimiter, including at exactly the line limit.
                    self.extra_cr = true;
                } else {
                    self.truncated = true;
                }
            }
        }
    }

    fn emit(&mut self) {
        let mut bytes = self.bytes.as_slice();
        if self.truncated {
            if let Err(error) = std::str::from_utf8(bytes) {
                if error.error_len().is_none() {
                    bytes = &bytes[..error.valid_up_to()];
                }
            }
        }
        let message = String::from_utf8_lossy(bytes).into_owned();
        self.collector.record(
            &self.service,
            self.generation,
            self.level,
            message,
            self.truncated,
        );
        self.bytes.clear();
        self.truncated = false;
        self.extra_cr = false;
    }

    fn finish(&mut self) {
        if self.extra_cr {
            self.truncated = true;
        }
        if !self.bytes.is_empty() || self.truncated {
            self.emit();
        }
    }
}

impl Drop for StreamCollector {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collector(capacity: usize, max_line_bytes: usize) -> LogCollector {
        LogCollector::new(LogOptions {
            capacity,
            max_line_bytes,
        })
        .unwrap()
    }

    fn stream(collector: &LogCollector) -> StreamCollector {
        StreamCollector {
            collector: collector.clone(),
            service: "api".into(),
            generation: 2,
            level: LogLevel::Info,
            bytes: Vec::new(),
            truncated: false,
            extra_cr: false,
        }
    }

    #[test]
    fn test_logging_framing_is_independent_of_every_chunk_boundary() {
        let bytes = b"first\r\n\n\xe4\xb8\xad\xe6\x96\x87\ninvalid:\xff\ntail\r";
        for split in 0..=bytes.len() {
            let collector = collector(100, 100);
            let mut stream = stream(&collector);
            stream.push(&bytes[..split]);
            stream.push(&bytes[split..]);
            drop(stream);
            let entries = collector.history().recent(None, 100);
            let expected = [
                "first",
                "",
                "\u{4e2d}\u{6587}",
                "invalid:\u{fffd}",
                "tail\r",
            ];
            assert_eq!(
                entries
                    .iter()
                    .map(|e| e.message.as_str())
                    .collect::<Vec<_>>(),
                expected,
                "split {split}"
            );
            assert!(entries.iter().all(|e| e.service == "api"
                && e.generation == 2
                && e.level == LogLevel::Info
                && !e.truncated));
        }
    }

    #[test]
    fn test_logging_long_line_truncates_once_and_recovers_at_next_newline() {
        let collector = collector(10, 4);
        let mut stream = stream(&collector);
        stream.push(b"abcd");
        for _ in 0..1000 {
            stream.push(b"discard this data");
        }
        assert_eq!(stream.bytes, b"abcd");
        assert!(collector.history().recent(None, 10).is_empty());
        stream.push(b"\nnext\r\ntail-overflow");
        drop(stream);
        let entries = collector.history().recent(None, 10);
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.message.as_str(), e.truncated))
                .collect::<Vec<_>>(),
            [("abcd", true), ("next", false), ("tail", true)]
        );
    }

    #[test]
    fn test_logging_truncation_does_not_split_a_valid_unicode_character() {
        let collector = collector(10, 3);
        let mut stream = stream(&collector);
        stream.push(b"a\xe4\xb8\xad\n");
        drop(stream);
        let entries = collector.history().recent(None, 10);
        assert_eq!(entries[0].message, "a");
        assert!(entries[0].truncated);
    }

    #[test]
    fn test_logging_crlf_at_limit_and_carriage_returns_in_content() {
        for (input, expected, truncated) in [
            (b"ab\r\n".as_slice(), "ab", false),
            (b"a\r\r\n", "a\r", false),
            (b"ab\r", "ab", true),
            (b"ab\rx\n", "ab", true),
            (b"a\r", "a\r", false),
        ] {
            for split in 0..=input.len() {
                let collector = collector(10, 2);
                let mut stream = stream(&collector);
                stream.push(&input[..split]);
                stream.push(&input[split..]);
                drop(stream);
                let entries = collector.history().recent(None, 10);
                assert_eq!(entries.len(), 1);
                assert_eq!(
                    (&*entries[0].message, entries[0].truncated),
                    (expected, truncated)
                );
            }
        }
    }

    #[test]
    fn test_logging_finishing_stream_is_idempotent() {
        let collector = collector(10, 10);
        let mut stream = stream(&collector);
        stream.push(b"tail");
        stream.finish();
        stream.finish();
        drop(stream);
        assert_eq!(collector.history().recent(None, 10).len(), 1);
        drop(self::stream(&collector));
        assert_eq!(collector.history().recent(None, 10).len(), 1);
    }

    #[tokio::test]
    async fn test_logging_history_eviction_filter_and_recent_order() {
        let collector = collector(3, 100);
        collector
            .collect(b"first\nsecond\n".as_slice(), "a".into(), 0, LogLevel::Info)
            .await
            .unwrap();
        collector
            .collect(b"third\n".as_slice(), "b".into(), 0, LogLevel::Error)
            .await
            .unwrap();
        collector
            .collect(b"fourth\n".as_slice(), "a".into(), 1, LogLevel::Info)
            .await
            .unwrap();
        let history = collector.history();
        assert_eq!(
            history
                .recent(None, usize::MAX)
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>(),
            ["second", "third", "fourth"]
        );
        assert_eq!(history.recent(Some("a"), 1)[0].message, "fourth");
        assert!(history.recent(Some("missing"), 10).is_empty());
        assert!(history.recent(None, 0).is_empty());
    }

    #[tokio::test]
    async fn test_logging_snapshot_subscription_does_not_repeat_history() {
        let collector = collector(10, 100);
        collector
            .collect(b"before\n".as_slice(), "b".into(), 0, LogLevel::Info)
            .await
            .unwrap();
        let (tail, mut live) = collector.history().subscribe_with_recent(Some("b"), 1);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].message, "before");
        collector
            .collect(b"after\n".as_slice(), "b".into(), 0, LogLevel::Info)
            .await
            .unwrap();
        assert_eq!(live.recv().await.unwrap().message, "after");
        let history = collector.history();
        drop(collector);
        let (tail, mut closed) = history.subscribe_with_recent(Some("b"), 1);
        assert_eq!(tail[0].message, "after");
        assert!(matches!(
            closed.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));
    }

    #[test]
    fn test_logging_options_reject_zero_and_excessive_resource_bounds() {
        for capacity in [0, 65_537, usize::MAX] {
            assert!(matches!(
                LogCollector::new(LogOptions {
                    capacity,
                    ..LogOptions::default()
                }),
                Err(LogOptionsError::InvalidCapacity)
            ));
        }
        for max_line_bytes in [0, 1_048_577, usize::MAX] {
            assert!(matches!(
                LogCollector::new(LogOptions {
                    max_line_bytes,
                    ..LogOptions::default()
                }),
                Err(LogOptionsError::InvalidLineSize)
            ));
        }
    }
}
