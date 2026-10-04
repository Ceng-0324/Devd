use std::{io, sync::Arc};

use colored::Color;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::broadcast,
};

use super::{LogEntry, LogLevel};

#[derive(Debug, Clone, Copy, Default)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LogFormatter {
    pub color: ColorMode,
}

impl LogFormatter {
    /// A UTC timestamp and complete prefix on every physical output line.
    /// Escape controls so child output cannot erase or forge terminal prefixes.
    pub fn format(&self, entry: &LogEntry) -> String {
        let service = self.paint(&escape(&entry.service), service_color(&entry.service));
        let (label, color) = match entry.level {
            LogLevel::Info => ("INFO", Color::Blue),
            LogLevel::Warn => ("WARN", Color::Yellow),
            LogLevel::Error => ("ERROR", Color::Red),
        };
        let level = self.paint(label, color);
        let marker = if entry.truncated { " [truncated]" } else { "" };
        format!(
            "[{}] [{service}] [{level}] {}{marker}\n",
            entry.timestamp.format("%H:%M:%S%.3fZ"),
            escape(&entry.message)
        )
    }

    fn paint(&self, text: &str, color: Color) -> String {
        let enabled = match self.color {
            ColorMode::Auto => colored::control::SHOULD_COLORIZE.should_colorize(),
            ColorMode::Always => true,
            ColorMode::Never => false,
        };
        // Use colored's palette while avoiding process-global color overrides.
        if enabled {
            format!("\x1b[{}m{text}\x1b[0m", color.to_fg_str())
        } else {
            text.into()
        }
    }
}

fn service_color(service: &str) -> Color {
    let palette = [
        Color::Cyan,
        Color::Green,
        Color::Magenta,
        Color::Yellow,
        Color::BrightBlue,
        Color::BrightRed,
    ];
    let hash = service.bytes().fold(0u64, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(u64::from(byte))
    });
    palette[(hash % palette.len() as u64) as usize]
}

fn escape(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() && character != '\t' {
            safe.extend(character.escape_default());
        } else {
            safe.push(character);
        }
    }
    safe
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct OutputSummary {
    pub entries: u64,
    pub dropped: u64,
}

/// One writer serializes complete entries until all producers close. Slow sinks
/// lose whole entries (reported as WARN); pipe capture and history keep running.
/// Keep only LogHistory, rather than a LogCollector, when waiting for closure.
pub async fn write_logs(
    mut writer: impl AsyncWrite + Unpin,
    mut entries: broadcast::Receiver<Arc<LogEntry>>,
    formatter: LogFormatter,
) -> io::Result<OutputSummary> {
    let mut summary = OutputSummary::default();
    loop {
        let line = match entries.recv().await {
            Ok(entry) => {
                summary.entries = summary.entries.saturating_add(1);
                formatter.format(&entry)
            }
            Err(broadcast::error::RecvError::Lagged(count)) => {
                summary.dropped = summary.dropped.saturating_add(count);
                formatter.format(&LogEntry {
                    timestamp: chrono::Utc::now(),
                    service: "devd".into(),
                    generation: 0,
                    level: LogLevel::Warn,
                    message: format!("log output skipped {count} entries: subscriber lagged"),
                    truncated: false,
                })
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await?;
    }
    writer.flush().await?;
    Ok(summary)
}
