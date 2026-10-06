use std::{
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, Context, Result};

/// Snapshots are configuration bytes, never a runtime/PID checkpoint.
pub(super) async fn save(config: &Path, state_dir: &Path, name: &str) -> Result<PathBuf> {
    let path = snapshot_path(state_dir, name)?;
    let bytes = read_regular_file(config, "configuration").await?;
    let directory = path.parent().context("snapshot path has no parent")?;
    tokio::fs::create_dir_all(directory)
        .await
        .with_context(|| format!("cannot create snapshot directory {}", directory.display()))?;
    reject_symlink_directory(directory).await?;
    create_new(path.clone(), bytes).await?;
    Ok(path)
}

pub(super) async fn restore(
    config: &Path,
    state_dir: &Path,
    name: &str,
    output: &Path,
) -> Result<PathBuf> {
    let source = snapshot_path(state_dir, name)?;
    let destination = output_path(config, output)?;
    reject_symlink_directory(source.parent().context("snapshot path has no parent")?).await?;
    let bytes = read_regular_file(&source, "snapshot").await?;
    create_new(destination.clone(), bytes).await?;
    Ok(destination)
}

fn snapshot_path(state_dir: &Path, name: &str) -> Result<PathBuf> {
    if name.len() > 64
        || !name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
        })
    {
        bail!("invalid snapshot name '{name}': use 1–64 lowercase ASCII letters, digits, '_', '-', or '.', starting with a letter, digit, or '_'");
    }
    Ok(state_dir.join("snapshots").join(format!("{name}.yml")))
}

fn output_path(config: &Path, output: &Path) -> Result<PathBuf> {
    let mut components = output.components();
    if !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || output.file_name() != Some(output.as_os_str())
    {
        bail!("--output must be a single filename in the configuration directory");
    }
    Ok(config
        .parent()
        .context("configuration path has no parent")?
        .join(output))
}

async fn reject_symlink_directory(path: &Path) -> Result<()> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .with_context(|| format!("cannot inspect snapshot directory {}", path.display()))?;
    if !metadata.file_type().is_dir() {
        bail!(
            "snapshot directory {} is not a regular directory",
            path.display()
        );
    }
    Ok(())
}

async fn read_regular_file(path: &Path, label: &'static str) -> Result<Vec<u8>> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(&path)
            .map_err(|error| {
                if error.raw_os_error() == Some(nix::libc::ELOOP) {
                    anyhow::anyhow!("{label} {} is not a regular file", path.display())
                } else {
                    anyhow::Error::new(error)
                        .context(format!("cannot open {label} {}", path.display()))
                }
            })?;
        if !file.metadata()?.file_type().is_file() {
            bail!("{label} {} is not a regular file", path.display());
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .with_context(|| format!("cannot read {label} {}", path.display()))?;
        Ok(bytes)
    })
    .await
    .context("snapshot read task failed")?
}

async fn create_new(path: PathBuf, bytes: Vec<u8>) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let directory = path.parent().context("destination has no parent")?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)
            .with_context(|| format!("cannot create temporary file in {}", directory.display()))?;
        temporary
            .write_all(&bytes)
            .with_context(|| format!("cannot write temporary file for {}", path.display()))?;
        temporary
            .as_file()
            .sync_all()
            .with_context(|| format!("cannot sync temporary file for {}", path.display()))?;
        temporary.persist_noclobber(&path).map_err(|error| {
            if error.error.kind() == io::ErrorKind::AlreadyExists {
                anyhow::anyhow!("{} already exists; choose another name", path.display())
            } else {
                anyhow::Error::new(error).context(format!("cannot create {}", path.display()))
            }
        })?;
        Ok(())
    })
    .await
    .context("snapshot file task failed")?
}
