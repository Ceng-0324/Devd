//! Capability-relative, exclusively locked incident files. New UUID files are
//! never overwritten; a partial JSONL file remains recognizable after a kill.
use super::{
    evidence::{self, Begin, End, Record},
    EndReason,
};
use crate::{config::DevdConfig, core::state_store::StateStore, platform::files};
use anyhow::{bail, Context, Result};
#[cfg(windows)]
use cap_fs_ext::MetadataExt as _;
use cap_std::fs::{Dir, MetadataExt, OpenOptions, OpenOptionsExt};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use tokio::sync::{mpsc, OwnedSemaphorePermit};

pub(super) const MAX_BYTES: usize = 2 * 1024 * 1024;
const KEEP: usize = 20;

pub(super) enum Work {
    Begin {
        capture: Box<Begin>,
        config: Arc<DevdConfig>,
        permit: Arc<OwnedSemaphorePermit>,
        finished: Arc<AtomicBool>,
        deadline: Instant,
    },
    End(End),
}

pub(crate) struct Storage {
    directory: Arc<Dir>,
    retained: VecDeque<String>,
    evicted: u64,
    _lock: Arc<files::FileLock>,
    gate: Arc<Mutex<()>>,
    // Never release the supervisor lease while blocking I/O still owns writes.
    _lease: Arc<StateStore>,
}

struct Artifact {
    file: cap_std::fs::File,
    directory: Arc<Dir>,
    name: String,
    bytes: usize,
    _permit: Arc<OwnedSemaphorePermit>,
}

impl Storage {
    pub(in crate::cli) fn reader(&self) -> super::query::Reader {
        super::query::Reader::live(
            self.directory.clone(),
            self.gate.clone(),
            self._lock.clone(),
            self._lease.clone(),
        )
    }

    pub(in crate::cli) fn open(state: &Path, lease: Arc<StateStore>) -> Result<Self> {
        let parent = Dir::open_ambient_dir(state, cap_std::ambient_authority())?;
        let builder = cap_std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use cap_std::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        match parent.create_dir_with("incidents", &builder) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let metadata = parent.symlink_metadata("incidents")?;
        if !metadata.is_dir() || is_link(&metadata) {
            bail!("incident directory is a link or not a directory");
        }
        let directory = Arc::new(open_directory(&parent, "incidents")?);
        let lock = open_file(&directory, ".lock", false)?;
        let lock =
            files::FileLock::acquire(lock.into_std(), false).context("incident storage is busy")?;
        let mut retained = vec![];
        for (count, entry) in directory.entries()?.enumerate() {
            if count >= 256 {
                bail!("incident directory exceeds 256 entries");
            }
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !managed_name(name) {
                continue;
            }
            let file = read_file(&directory, name)?;
            let metadata = file.metadata()?;
            if metadata.len() > MAX_BYTES as u64 {
                bail!("incident exceeds 2 MiB");
            }
            retained.push((metadata.modified()?, name.to_owned()));
        }
        retained.sort();
        let mut storage = Self {
            directory,
            retained: retained.into_iter().map(|(_, name)| name).collect(),
            evicted: 0,
            _lock: Arc::new(lock),
            gate: Arc::new(Mutex::new(())),
            _lease: lease,
        };
        storage.prune(KEEP, &BTreeMap::new())?;
        Ok(storage)
    }

    fn prune(&mut self, keep: usize, active: &BTreeMap<String, Artifact>) -> Result<()> {
        while self.retained.len() > keep {
            let index = self
                .retained
                .iter()
                .position(|name| !active.contains_key(name.trim_end_matches(".jsonl")))
                .context("all retained incidents are active")?;
            let name = &self.retained[index];
            // Retention only removes validated, single-link managed UUID files.
            // Unknown files and all log/event/snapshot directories are untouched.
            drop(read_file(&self.directory, name)?);
            self.directory.remove_file(name)?;
            self.retained.remove(index);
            self.evicted += 1;
        }
        Ok(())
    }

    pub(super) fn run(
        mut self,
        mut receive: mpsc::Receiver<Work>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<()> {
        let mut active: BTreeMap<String, Artifact> = BTreeMap::new();
        let gate = self.gate.clone();
        while let Some(work) = receive.blocking_recv() {
            let _guard = gate
                .lock()
                .map_err(|_| anyhow::anyhow!("incident storage mutex poisoned"))?;
            match work {
                Work::Begin {
                    capture,
                    config,
                    permit,
                    finished,
                    deadline,
                } => {
                    if cancelled.load(Ordering::Acquire) {
                        continue;
                    }
                    self.prune(KEEP - 1, &active)?;
                    let name = format!("{}.jsonl", capture.id);
                    if !managed_name(&name) || active.len() >= super::MAX_PENDING {
                        bail!("invalid incident admission");
                    }
                    let file = open_file(&self.directory, &name, true)?;
                    self.retained.push_back(name.clone());
                    let mut artifact = Artifact {
                        file,
                        directory: self.directory.clone(),
                        name,
                        bytes: 0,
                        _permit: permit,
                    };
                    artifact.append(&Record::Begin {
                        capture: &capture,
                        configuration: evidence::configuration(&config),
                        limitations: evidence::LIMITATIONS,
                        retention_evicted_this_recorder: self.evicted,
                    })?;
                    // Beginning is durable before any fresh path inspection.
                    let resources = evidence::resources(
                        &capture,
                        &config,
                        &capture.identity.state_dir,
                        deadline,
                        &finished,
                        &cancelled,
                    );
                    artifact.append(&Record::Resources(resources))?;
                    active.insert(capture.id.clone(), artifact);
                }
                Work::End(end) => {
                    if let Some(mut artifact) = active.remove(&end.id) {
                        artifact.append(&Record::End(&end))?;
                    }
                }
            }
        }
        // Dropping the async collector closes its queue. Already accepted
        // beginnings may finish only as explicitly cancelled, never complete.
        let _guard = gate
            .lock()
            .map_err(|_| anyhow::anyhow!("incident storage mutex poisoned"))?;
        for (id, mut artifact) in active {
            artifact.append(&Record::End(&End {
                id,
                reason: EndReason::Cancelled,
                observed_at: chrono::Utc::now(),
                following: vec![],
                following_omitted: None,
                merged_triggers: None,
                skipped_events: None,
                statistics: None,
            }))?;
        }
        Ok(())
    }
}

impl Artifact {
    fn append(&mut self, record: &impl serde::Serialize) -> Result<()> {
        let mut bytes = Limited(Vec::new(), MAX_BYTES.saturating_sub(self.bytes + 1));
        serde_json::to_writer(&mut bytes, record).context("incident evidence exceeds 2 MiB")?;
        bytes.0.push(b'\n');
        self.verify_path()?;
        self.file.write_all(&bytes.0)?;
        self.file.sync_all()?;
        self.verify_path()?;
        self.bytes += bytes.0.len();
        Ok(())
    }

    fn verify_path(&self) -> Result<()> {
        let held = self.file.metadata()?;
        check_metadata(&held)?;
        let opened = read_file(&self.directory, &self.name)?;
        let current = opened.metadata()?;
        if (held.dev(), held.ino()) != (current.dev(), current.ino()) {
            bail!("incident file changed during capture");
        }
        Ok(())
    }
}

struct Limited(Vec<u8>, usize);
impl Write for Limited {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.len() > self.1.saturating_sub(self.0.len()) {
            return Err(io::Error::other("incident size limit"));
        }
        self.0.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn managed_name(name: &str) -> bool {
    name.strip_suffix(".jsonl")
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id))
}

fn is_link(metadata: &cap_std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.file_type().is_symlink()
    }
    #[cfg(windows)]
    {
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
}

fn check_metadata(metadata: &cap_std::fs::Metadata) -> Result<()> {
    if !metadata.is_file() || is_link(metadata) || metadata.nlink() != 1 {
        bail!("incident storage requires a regular single-link file");
    }
    Ok(())
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    options
}

pub(super) fn open_directory(parent: &Dir, name: &str) -> Result<Dir> {
    let mut options = options();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_DIRECTORY);
    #[cfg(windows)]
    options.custom_flags(
        windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT
            | windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS,
    );
    let file = parent.open_with(name, &options)?.into_std();
    let metadata = file.metadata()?;
    if !metadata.is_dir() || files::is_link(&metadata) {
        bail!("incident directory changed to a link or file");
    }
    Ok(Dir::from_std_file(file))
}

pub(super) fn read_file(directory: &Dir, name: &str) -> Result<cap_std::fs::File> {
    check_metadata(&directory.symlink_metadata(name)?)?;
    let file = directory.open_with(name, options().read(true))?;
    check_metadata(&file.metadata()?)?;
    Ok(file)
}

fn open_file(directory: &Dir, name: &str, new: bool) -> Result<cap_std::fs::File> {
    match directory.symlink_metadata(name) {
        Ok(metadata) => {
            check_metadata(&metadata)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut options = options();
    options.read(true).write(true);
    if new {
        options.create_new(true);
    } else {
        options.create(true);
    }
    let file = directory.open_with(name, &options)?;
    check_metadata(&file.metadata()?)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn test_incident_storage_locks_links_no_clobber_and_active_retention() {
        let root = tempfile::tempdir().unwrap();
        let lease = Arc::new(
            StateStore::open(&root.path().join("state.json"))
                .await
                .unwrap(),
        );
        let mut storage = Storage::open(root.path(), lease.clone()).unwrap();
        assert!(Storage::open(root.path(), lease.clone()).is_err());
        let id = uuid::Uuid::new_v4().to_string();
        let name = format!("{id}.jsonl");
        let mut file = open_file(&storage.directory, &name, true).unwrap();
        file.write_all(b"old").unwrap();
        assert!(open_file(&storage.directory, &name, true).is_err());
        assert_eq!(
            fs::read(root.path().join("incidents").join(&name)).unwrap(),
            b"old"
        );
        let permit = Arc::new(tokio::sync::Semaphore::new(1))
            .try_acquire_owned()
            .unwrap();
        let active = BTreeMap::from([(
            id,
            Artifact {
                file,
                directory: storage.directory.clone(),
                name: name.clone(),
                bytes: 3,
                _permit: Arc::new(permit),
            },
        )]);
        storage.retained.push_back(name.clone());
        for _ in 0..21 {
            let next = format!("{}.jsonl", uuid::Uuid::new_v4());
            open_file(&storage.directory, &next, true).unwrap();
            storage.retained.push_back(next);
        }
        storage.prune(KEEP, &active).unwrap();
        assert_eq!(storage.retained.len(), 20);
        assert_eq!(storage.evicted, 2);
        assert!(storage.directory.symlink_metadata(&name).is_ok());
        drop(active);
        let target = root.path().join("incidents").join(&name);
        fs::hard_link(&target, root.path().join("extra")).unwrap();
        assert!(read_file(&storage.directory, &name).is_err());
        drop(storage);
        assert!(Storage::open(root.path(), lease.clone()).is_err());
        fs::remove_file(root.path().join("extra")).unwrap();
        fs::remove_file(&target).unwrap();
        let unrelated = root.path().join("unrelated");
        fs::write(&unrelated, "private").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&unrelated, &target).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&unrelated, &target)
            .expect("symlink privileges required");
        assert!(Storage::open(root.path(), lease).is_err());
        assert_eq!(fs::read_to_string(&unrelated).unwrap(), "private");
    }

    #[tokio::test]
    async fn test_incident_storage_rejects_redirected_directory_and_special_lock() {
        let root = tempfile::tempdir().unwrap();
        let lease = Arc::new(
            StateStore::open(&root.path().join("state.json"))
                .await
                .unwrap(),
        );
        let target = root.path().join("elsewhere");
        fs::create_dir(&target).unwrap();
        let directory = root.path().join("incidents");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &directory).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&target, &directory)
            .expect("symlink privileges required");
        assert!(Storage::open(root.path(), lease.clone()).is_err());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        #[cfg(unix)]
        fs::remove_file(&directory).unwrap();
        #[cfg(windows)]
        fs::remove_dir(&directory).unwrap();
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join(".lock")).unwrap();
        assert!(Storage::open(root.path(), lease.clone()).is_err());
        fs::remove_dir(directory.join(".lock")).unwrap();
        #[cfg(unix)]
        {
            nix::unistd::mkfifo(&directory.join(".lock"), nix::sys::stat::Mode::S_IRUSR).unwrap();
            assert!(Storage::open(root.path(), lease).is_err());
        }
    }

    #[tokio::test]
    async fn test_incident_size_limit_fails_before_appending_and_preserves_prefix() {
        let root = tempfile::tempdir().unwrap();
        let lease = Arc::new(
            StateStore::open(&root.path().join("state.json"))
                .await
                .unwrap(),
        );
        let storage = Storage::open(root.path(), lease).unwrap();
        let name = format!("{}.jsonl", uuid::Uuid::new_v4());
        let file = open_file(&storage.directory, &name, true).unwrap();
        let permit = Arc::new(tokio::sync::Semaphore::new(1))
            .try_acquire_owned()
            .unwrap();
        let mut artifact = Artifact {
            file,
            directory: storage.directory.clone(),
            name: name.clone(),
            bytes: 0,
            _permit: Arc::new(permit),
        };
        artifact.append(&"begin").unwrap();
        assert!(artifact.append(&"x".repeat(MAX_BYTES)).is_err());
        assert_eq!(
            fs::read_to_string(root.path().join("incidents").join(&name)).unwrap(),
            "\"begin\"\n"
        );
    }

    #[tokio::test]
    async fn test_incident_writer_rejects_replaced_name_without_writing_new_file() {
        let root = tempfile::tempdir().unwrap();
        let lease = Arc::new(
            StateStore::open(&root.path().join("state.json"))
                .await
                .unwrap(),
        );
        let storage = Storage::open(root.path(), lease).unwrap();
        let name = format!("{}.jsonl", uuid::Uuid::new_v4());
        let file = open_file(&storage.directory, &name, true).unwrap();
        let permit = Arc::new(tokio::sync::Semaphore::new(1))
            .try_acquire_owned()
            .unwrap();
        let mut artifact = Artifact {
            file,
            directory: storage.directory.clone(),
            name: name.clone(),
            bytes: 0,
            _permit: Arc::new(permit),
        };
        artifact.append(&"begin").unwrap();
        let original = root.path().join("incidents").join(&name);
        let moved = root.path().join("moved");
        fs::rename(&original, &moved).unwrap();
        fs::write(&original, "replacement").unwrap();
        assert!(artifact.append(&"end").is_err());
        assert_eq!(fs::read_to_string(&original).unwrap(), "replacement");
        assert_eq!(fs::read_to_string(&moved).unwrap(), "\"begin\"\n");
    }
}
