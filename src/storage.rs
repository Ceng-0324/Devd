//! Locked, bounded rotating JSONL files shared by logs and lifecycle events.
use crate::platform::files;
use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
const MAX_ARCHIVES: u16 = 100;

#[derive(Debug, Clone, Copy)]
pub struct StorageOptions {
    pub max_file_bytes: u64,
    /// Rotated archives in addition to the current file.
    pub keep: u16,
}

impl Default for StorageOptions {
    fn default() -> Self {
        Self {
            max_file_bytes: 10 * 1024 * 1024,
            keep: 3,
        }
    }
}

pub(crate) struct JsonlStorage {
    directory: PathBuf,
    options: StorageOptions,
    file: File,
    size: u64,
    _lock: File,
    max_record_bytes: usize,
}

impl JsonlStorage {
    /// Open only after acquiring the supervisor's state lock, before spawning
    /// services. Existing complete records survive across supervisor runs.
    pub fn open(
        directory: &Path,
        options: StorageOptions,
        max_record_bytes: usize,
    ) -> io::Result<(Self, u64)> {
        if options.max_file_bytes == 0 || options.max_file_bytes > 1024 * 1024 * 1024 {
            return Err(invalid("JSONL file size must be between 1 byte and 1 GiB"));
        }
        if !(1..=MAX_ARCHIVES).contains(&options.keep) {
            return Err(invalid("JSONL archive count must be between 1 and 100"));
        }
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        match builder.create(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        check_directory(directory)?;
        let lock = lock(directory, true)?;
        // Check every managed path before repairing or rotating any file.
        for index in 0..=MAX_ARCHIVES {
            check_file(&record_path(directory, index))?;
        }
        let mut file = open_file(&record_path(directory, 0), true)?;
        let removed = repair_tail(&mut file, max_record_bytes)?;
        let size = file.metadata()?.len();
        let mut storage = Self {
            directory: directory.into(),
            options,
            file,
            size,
            _lock: lock,
            max_record_bytes,
        };
        // A reduced retention setting also applies to archives from older runs.
        for index in options.keep + 1..=MAX_ARCHIVES {
            remove_if_present(&record_path(directory, index))?;
        }
        if storage.size > options.max_file_bytes {
            storage.rotate()?;
        }
        Ok((storage, removed))
    }

    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    pub fn append(&mut self, entry: &impl serde::Serialize) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(entry).map_err(io::Error::other)?;
        bytes.push(b'\n');
        if bytes.len() > self.max_record_bytes || bytes.len() as u64 > self.options.max_file_bytes {
            return Err(invalid(
                "stored JSONL record exceeds the JSONL file size limit",
            ));
        }
        if self.size + bytes.len() as u64 > self.options.max_file_bytes {
            self.rotate()?;
        }
        self.file.write_all(&bytes)?;
        self.size += bytes.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.sync_data()?;
        for index in (0..self.options.keep).rev() {
            let source = record_path(&self.directory, index);
            let destination = record_path(&self.directory, index + 1);
            if check_file(&source)? {
                check_file(&destination)?;
                fs::rename(source, destination)?;
            }
        }
        self.file = open_file(&record_path(&self.directory, 0), true)?;
        self.size = 0;
        Ok(())
    }
}

pub(crate) enum Record<'a> {
    Complete(&'a [u8]),
    Unfinished(u64),
}

pub(crate) fn scan(
    directory: &Path,
    max_record_bytes: usize,
    mut visit: impl FnMut(Record<'_>) -> io::Result<()>,
) -> io::Result<()> {
    check_directory(directory)?;
    let _lock = lock(directory, false)?;
    for index in (0..=MAX_ARCHIVES).rev() {
        let path = record_path(directory, index);
        if !check_file(&path)? {
            continue;
        }
        let mut reader = BufReader::new(open_file(&path, false)?);
        let mut line = Vec::new();
        loop {
            line.clear();
            Read::by_ref(&mut reader)
                .take(max_record_bytes as u64 + 1)
                .read_until(b'\n', &mut line)?;
            if line.is_empty() {
                break;
            }
            if line.len() > max_record_bytes {
                return Err(invalid(format!(
                    "oversized JSONL record in {}",
                    path.display()
                )));
            }
            if line.last() != Some(&b'\n') {
                if index == 0 {
                    visit(Record::Unfinished(line.len() as u64))?;
                    break;
                }
                return Err(invalid(format!(
                    "unfinished JSONL record in {}",
                    path.display()
                )));
            }
            visit(Record::Complete(&line))?;
        }
    }
    Ok(())
}

pub(crate) fn record_path(directory: &Path, index: u16) -> PathBuf {
    directory.join(if index == 0 {
        "current.jsonl".into()
    } else {
        format!("archive-{index}.jsonl")
    })
}

fn check_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || files::is_link(&metadata) {
        return Err(invalid(format!(
            "not a JSONL directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn check_file(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if files::regular(&metadata) => {
            files::open_regular(path, false, true)?;
            Ok(true)
        }
        Ok(_) => Err(invalid(format!(
            "not a regular JSONL file: {}",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn open_file(path: &Path, write: bool) -> io::Result<File> {
    let mut file = files::open_regular(path, write, true)?;
    if write {
        file.seek(SeekFrom::End(0))?;
    }
    Ok(file)
}

fn lock(directory: &Path, write: bool) -> io::Result<File> {
    let file = open_file(&directory.join(".lock"), write)?;
    let result = if write {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    result.map_err(|error| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("stored history is in use; stop the supervisor or use a live query: {error}"),
        )
    })?;
    Ok(file)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn repair_tail(file: &mut File, max_record_bytes: usize) -> io::Result<u64> {
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(0);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(0);
    }
    let scan = length.min(max_record_bytes as u64);
    file.seek(SeekFrom::End(-(scan as i64)))?;
    let mut tail = Vec::with_capacity(scan as usize);
    file.take(scan).read_to_end(&mut tail)?;
    let removed = match tail.iter().rposition(|byte| *byte == b'\n') {
        Some(index) => scan - index as u64 - 1,
        None if scan == length => scan,
        None => {
            return Err(invalid(
                "unfinished stored JSONL record exceeds the size limit",
            ))
        }
    };
    if removed > 0 {
        file.set_len(length - removed)?;
        // Truncation preserves the old cursor. On Windows writes use that
        // position; continuing there would insert a zero-filled gap.
        file.seek(SeekFrom::End(0))?;
    }
    Ok(removed)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
