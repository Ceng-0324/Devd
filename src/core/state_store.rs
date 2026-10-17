use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use super::service_manager::{RuntimeSnapshot, ServiceManagerError};

pub(crate) struct StateStore {
    path: PathBuf,
    temporary: PathBuf,
    lock: Arc<crate::platform::files::FileLock>,
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
        let lock_path: PathBuf = lock_path.into();
        let file = tokio::task::spawn_blocking(move || {
            crate::platform::files::open_regular(&lock_path, true, true)
        })
        .await
        .map_err(|source| state_error(path, io::Error::other(source)))?
        .map_err(|source| state_error(path, source))?;
        let lock = crate::platform::files::FileLock::acquire(file, false)
            .map_err(|source| state_error(path, source))?;
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
            let mut file = crate::platform::files::open_regular(&temporary, true, true)?;
            file.set_len(0)?;
            file.write_all(&bytes)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::service_manager::RuntimeSnapshot;

    #[tokio::test]
    async fn test_write_rejects_temporary_symlink_without_touching_target() {
        #[cfg(unix)]
        use std::os::unix::fs::symlink;
        #[cfg(windows)]
        use std::os::windows::fs::symlink_file as symlink;

        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("services.json");
        let target = root.path().join("outside.json");
        std::fs::write(&target, "keep me").unwrap();
        symlink(&target, root.path().join("services.json.tmp")).unwrap();
        let store = StateStore::open(&state).await.unwrap();

        let error = store
            .write(&RuntimeSnapshot {
                supervisor_pid: 1,
                event_run_id: None,
                services: Default::default(),
            })
            .await
            .unwrap_err();

        assert!(error.to_string().contains("not a regular file"));
        assert_eq!(std::fs::read_to_string(target).unwrap(), "keep me");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_open_rejects_lock_fifo_without_blocking() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("services.json");
        let lock = root.path().join("services.json.lock");
        nix::unistd::mkfifo(
            &lock,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(1), StateStore::open(&state))
                .await
                .expect("FIFO state lock must not block supervisor startup");
        assert!(matches!(
            result,
            Err(ServiceManagerError::StateIo { source, .. })
                if source.kind() == io::ErrorKind::InvalidData
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_open_rejects_linked_lock_files() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("services.json");
        let target = root.path().join("outside.lock");
        std::fs::write(&target, "keep me").unwrap();
        symlink(&target, root.path().join("services.json.lock")).unwrap();

        let error = match StateStore::open(&state).await {
            Ok(_) => panic!("linked state lock must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            ServiceManagerError::StateIo { source, .. }
                if source.kind() == io::ErrorKind::InvalidData
        ));
        assert_eq!(std::fs::read_to_string(target).unwrap(), "keep me");
    }
}
