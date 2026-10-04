mod collector;
mod output;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use collector::{LogCollector, LogHistory, LogOptions, LogOptionsError};
pub use output::{write_logs, ColorMode, LogFormatter, OutputSummary};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
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
