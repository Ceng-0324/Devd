//! Shared live/offline export cancellation, bounded encoding and no-clobber publication.
use anyhow::{bail, Context, Result};
#[cfg(windows)]
use cap_fs_ext::MetadataExt;
#[cfg(unix)]
use cap_std::fs::MetadataExt;
use std::io::Read;
use std::{
    io::{self, Write},
    path::Path,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
pub(in crate::cli) const MAX_REPORT_BYTES: usize = 16 * 1024 * 1024;
pub(in crate::cli) const DEADLINE: Duration = Duration::from_secs(30);

// 0 = collecting/preparing; 1 = cancelled; 2 = publication accepted. The CAS
// serializes cancellation with publication; cancelled workers cannot publish late.
#[derive(Clone)]
pub(in crate::cli) struct Control {
    phase: Arc<AtomicU8>,
    deadline: Instant,
}
impl Control {
    #[cfg(test)]
    pub(in crate::cli) fn expired() -> Self {
        Self {
            phase: Arc::new(AtomicU8::new(0)),
            deadline: Instant::now(),
        }
    }

    pub(in crate::cli) fn new() -> Self {
        Self {
            phase: Arc::new(AtomicU8::new(0)),
            deadline: Instant::now() + DEADLINE,
        }
    }
    pub(in crate::cli) fn check(&self) -> io::Result<()> {
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
    pub(in crate::cli) fn cancel(&self) -> bool {
        self.phase
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub(in crate::cli) fn publish(&self) -> io::Result<()> {
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
pub(in crate::cli) struct CancelOnDrop(pub(in crate::cli) Control);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(in crate::cli) struct BoundedBytes(pub(in crate::cli) Vec<u8>);
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

pub(in crate::cli) fn encode(report: &impl serde::Serialize) -> Result<Vec<u8>> {
    let mut bytes = BoundedBytes(Vec::new());
    serde_json::to_writer_pretty(&mut bytes, report).context("cannot encode diagnostic report")?;
    Ok(bytes.0)
}

pub(in crate::cli) fn publish_report(
    path: &Path,
    report: &impl serde::Serialize,
    control: &Control,
) -> Result<()> {
    control.check()?;
    #[cfg(windows)]
    super::super::snapshot::validate_windows_filename(path)?;
    let bytes = encode(report)?;
    control.check()?;
    let directory = path.parent().context("destination has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    for chunk in bytes.chunks(64 * 1024) {
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

pub(in crate::cli) struct ReadFile {
    pub bytes: Vec<u8>,
    /// Identity of the same open file whose bytes were read and revalidated.
    pub identity: [u64; 2],
}

pub(in crate::cli) fn read_regular(
    path: &Path,
    control: &Control,
    limit: usize,
) -> Result<ReadFile> {
    control.check()?;
    let mut file = cap_std::fs::File::from_std(
        crate::platform::files::open_regular(path, false, true)
            .context("input must be a regular, single-link file")?,
    );
    let before = file.metadata()?;
    if before.len() > limit as u64 {
        bail!("input exceeds {limit} byte limit")
    }
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        control.check()?;
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if bytes.len() + n > limit {
            bail!("input exceeds {limit} byte limit")
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    control.check()?;
    let same = |m: &cap_std::fs::Metadata| {
        (m.dev(), m.ino(), m.len(), m.modified().ok())
            == (
                before.dev(),
                before.ino(),
                before.len(),
                before.modified().ok(),
            )
    };
    let reopened =
        cap_std::fs::File::from_std(crate::platform::files::open_regular(path, false, true)?);
    if !same(&file.metadata()?) || !same(&reopened.metadata()?) {
        bail!("input changed during read; retry with a stable copy")
    }
    Ok(ReadFile {
        bytes,
        identity: [before.dev(), before.ino()],
    })
}
