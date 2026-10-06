use std::{
    fs::{File, Metadata, OpenOptions},
    io,
    path::Path,
};

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
