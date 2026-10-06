mod collector;
mod output;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use collector::{LogCollector, LogHistory, LogOptions, LogOptionsError};
pub use output::{write_logs, ColorMode, LogFormatter, OutputSummary};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// Optional predicates for retained and live log entries.
/// Time is an inclusive UTC lower bound; text matches the raw message literally.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogFilter {
    pub level: Option<LogLevel>,
    pub since: Option<DateTime<Utc>>,
    pub grep: Option<String>,
}

impl LogFilter {
    /// Apply the same predicates to a history entry or a live entry.
    pub fn matches(&self, entry: &LogEntry) -> bool {
        self.level.is_none_or(|level| entry.level == level)
            && self.since.is_none_or(|since| entry.timestamp >= since)
            && self
                .grep
                .as_ref()
                .is_none_or(|needle| entry.message.contains(needle))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub service: String,
    pub generation: u32,
    pub level: LogLevel,
    pub message: String,
    pub truncated: bool,
}
