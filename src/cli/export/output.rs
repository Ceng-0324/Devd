//! Shared live/offline export cancellation, bounded encoding and no-clobber publication.
use super::Report;
use anyhow::{Context, Result};
use std::{
    io::{self, Write},
    path::Path,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
pub(super) const MAX_REPORT_BYTES: usize = 16 * 1024 * 1024;
pub(super) const DEADLINE: Duration = Duration::from_secs(30);

// 0 = collecting/preparing; 1 = cancelled; 2 = publication accepted. The CAS
// serializes cancellation with publication; cancelled workers cannot publish late.
#[derive(Clone)]
pub(super) struct Control {
    phase: Arc<AtomicU8>,
    deadline: Instant,
}
impl Control {
    #[cfg(test)]
    pub(super) fn expired() -> Self {
        Self {
            phase: Arc::new(AtomicU8::new(0)),
            deadline: Instant::now(),
        }
    }

    pub(super) fn new() -> Self {
        Self {
            phase: Arc::new(AtomicU8::new(0)),
            deadline: Instant::now() + DEADLINE,
        }
    }
    pub(super) fn check(&self) -> io::Result<()> {
        if self.phase.load(Ordering::Acquire) == 1 {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "export cancelled",
            ));
        }
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "export exceeded 30s deadline",
            ));
        }
        Ok(())
    }
    pub(super) fn cancel(&self) -> bool {
        self.phase
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub(super) fn publish(&self) -> io::Result<()> {
        self.check()?;
        self.phase
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    "export cancelled before publication",
                )
            })?;
        Ok(())
    }
}
pub(super) struct CancelOnDrop(pub(super) Control);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct BoundedBytes(Vec<u8>);
impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_REPORT_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("diagnostic report exceeds 16 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn publish_report(path: &Path, report: &Report, control: &Control) -> Result<()> {
    control.check()?;
    #[cfg(windows)]
    super::super::snapshot::validate_windows_filename(path)?;
    let mut bytes = BoundedBytes(Vec::new());
    serde_json::to_writer_pretty(&mut bytes, report).context("cannot encode diagnostic report")?;
    control.check()?;
    let directory = path.parent().context("destination has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    for chunk in bytes.0.chunks(64 * 1024) {
        control.check()?;
        temporary.write_all(chunk)?;
    }
    temporary.as_file().sync_all()?;
    control.publish()?;
    temporary.persist_noclobber(path).map_err(|error| {
        if error.error.kind() == io::ErrorKind::AlreadyExists {
            anyhow::anyhow!("{} already exists; choose another name", path.display())
        } else {
            anyhow::Error::new(error.error).context("cannot publish diagnostic report")
        }
    })?;
    Ok(())
}
