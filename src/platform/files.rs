use std::{
    fs::{File, Metadata, OpenOptions},
    io,
    path::Path,
};

/// Release the lock explicitly before closing the last owned handle. A child
/// being spawned on another thread can temporarily inherit the open description
/// before exec closes it; close alone would keep its flock alive until then.
pub(crate) struct FileLock(File);

impl FileLock {
    pub(crate) fn acquire(file: File, shared: bool) -> io::Result<Self> {
        if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        }
        .map_err(io::Error::from)?;
        Ok(Self(file))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Reject special files and links both before open and on the opened handle.
/// Unix nonblocking/no-follow flags also prevent FIFOs from blocking startup.
pub(crate) fn open_regular(path: &Path, write: bool, single_link: bool) -> io::Result<File> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !regular(&metadata) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a regular file",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = OpenOptions::new();
    // A Windows append handle lacks FILE_WRITE_DATA, required by set_len when
    // repairing incomplete logs. Callers position writable files explicitly.
    options.read(true).write(write).create(write);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .append(write)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    if !regular(&file.metadata()?) || (single_link && !has_single_link(&file)?) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular single-link file",
        ));
    }
    Ok(file)
}

pub(crate) fn regular(metadata: &Metadata) -> bool {
    metadata.is_file() && !is_link(metadata)
}

pub(crate) fn is_link(metadata: &Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.file_type().is_symlink()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
}

fn has_single_link(file: &File) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(file.metadata()?.nlink() == 1)
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: file owns the handle; info is a correctly sized output buffer.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.nNumberOfLinks == 1)
    }
}

/// Consume a verified directory capability without following a replacement
/// ambient path. cap-std's Windows remove_open_dir_all closes its handle before
/// std::fs deletion, so use delete-capable pinned handles on that platform.
pub(crate) fn remove_owned_directory(dir: cap_std::fs::Dir) -> io::Result<()> {
    #[cfg(unix)]
    {
        dir.remove_open_dir_all()
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
        };
        fn remove(file: File) -> io::Result<()> {
            let info = FILE_DISPOSITION_INFO { DeleteFile: 1 };
            // The caller owns a DELETE-capable handle until the OS accepts
            // disposition; no close-then-reopen race against a replaced path.
            if unsafe {
                SetFileInformationByHandle(
                    file.as_raw_handle(),
                    FileDispositionInfo,
                    (&info as *const FILE_DISPOSITION_INFO).cast(),
                    std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        // Drop the enumeration handle before disposing of the parent.
        let entries = dir.entries()?.collect::<io::Result<Vec<_>>>()?;
        for entry in entries {
            let metadata = entry.metadata()?;
            use cap_std::fs::MetadataExt;
            if metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                != 0
            {
                return Err(io::Error::other("cleanup tree changed to a reparse point"));
            }
            if metadata.is_dir() {
                remove_owned_directory(open_owned_directory(&dir, Path::new(&entry.file_name()))?)?;
            } else if metadata.is_file() {
                let file = dir
                    .open_with(entry.file_name(), &delete_options())?
                    .into_std();
                if !regular(&file.metadata()?) || !has_single_link(&file)? {
                    return Err(io::Error::other("cleanup file identity changed"));
                }
                remove(file)?;
            } else {
                return Err(io::Error::other("cleanup tree contains a special file"));
            }
        }
        remove(dir.into_std_file())
    }
}

/// Open with DELETE access from the beginning on Windows. cap-std directory
/// handles deny delete sharing, so reopening an ordinary handle later would
/// require a dangerous close/open gap.
pub(crate) fn open_owned_directory(
    parent: &cap_std::fs::Dir,
    name: &Path,
) -> io::Result<cap_std::fs::Dir> {
    #[cfg(unix)]
    {
        parent.open_dir(name)
    }
    #[cfg(windows)]
    {
        let file = parent.open_with(name, &delete_options())?.into_std();
        let metadata = file.metadata()?;
        if !metadata.is_dir() || is_link(&metadata) {
            return Err(io::Error::other(
                "cleanup directory changed to a link or file",
            ));
        }
        Ok(cap_std::fs::Dir::from_std_file(file))
    }
}

#[cfg(windows)]
fn delete_options() -> cap_std::fs::OpenOptions {
    use cap_std::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .read(true)
        .access_mode(DELETE | FILE_GENERIC_READ)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    options
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn test_file_lock_releases_with_an_inherited_open_description() {
        let temporary = tempfile::NamedTempFile::new().unwrap();
        let file = temporary.reopen().unwrap();
        let inherited = file.try_clone().unwrap();
        let lock = std::sync::Arc::new(FileLock::acquire(file, false).unwrap());
        let lease = lock.clone();
        drop(lock);
        assert!(FileLock::acquire(temporary.reopen().unwrap(), false).is_err());
        drop(lease);
        let _next = FileLock::acquire(temporary.reopen().unwrap(), false).unwrap();
        drop(inherited);
    }
}
