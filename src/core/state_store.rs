use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use nix::fcntl::{Flock, FlockArg};

use super::service_manager::{RuntimeSnapshot, ServiceManagerError};

pub(super) struct StateStore {
    path: PathBuf,
    temporary: PathBuf,
    lock: Arc<Flock<std::fs::File>>,
}

impl StateStore {
    pub async fn open(path: &Path) -> Result<Self, ServiceManagerError> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| state_error(path, source))?;
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(Path::new(&lock_path))
            .await
            .map_err(|source| state_error(path, source))?
            .into_std()
            .await;
        let lock = Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, source)| {
            state_error(path, io::Error::from_raw_os_error(source as i32))
        })?;
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        Ok(Self {
            path: path.into(),
            temporary: temporary.into(),
            lock: Arc::new(lock),
        })
    }

    pub async fn write(&self, snapshot: &RuntimeSnapshot) -> Result<(), ServiceManagerError> {
        let bytes = serde_json::to_vec_pretty(snapshot)
            .map_err(|source| state_error(&self.path, io::Error::other(source)))?;
        let path = self.path.clone();
        let temporary = self.temporary.clone();
        let lease = self.lock.clone();
        // A cancelled write can continue on the blocking pool. Keep the lock
        // until atomic replacement completes, including after its caller drops.
        tokio::task::spawn_blocking(move || {
            let _lease = lease;
            std::fs::write(&temporary, bytes)?;
            std::fs::rename(&temporary, &path)
        })
        .await
        .map_err(|source| state_error(&self.path, io::Error::other(source)))?
        .map_err(|source| state_error(&self.path, source))
    }
}

fn state_error(path: &Path, source: io::Error) -> ServiceManagerError {
    ServiceManagerError::StateIo {
        path: path.into(),
        source,
    }
}
