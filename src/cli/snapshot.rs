use std::{
    io::{self, Read, Write},
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
    if !metadata.file_type().is_dir() || crate::platform::files::is_link(&metadata) {
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
        let mut file =
            crate::platform::files::open_regular(&path, false, false).map_err(|error| {
                if error.kind() == io::ErrorKind::InvalidData {
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
    #[cfg(windows)]
    validate_windows_filename(&path)?;
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

#[cfg(windows)]
fn validate_windows_filename(path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid Windows filename")?;
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    let device = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    if device
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|ch| ch.is_control() || "<>:\"/\\|?*".contains(ch))
    {
        bail!("invalid or reserved Windows filename: {name}");
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[tokio::test]
    async fn test_snapshot_rejects_windows_devices_and_alternate_streams() {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "con.yml",
            "NUL",
            "lpt1.yml",
            "COM¹.yml",
            "nul .yml",
            "safe.yml:stream",
            "trailing.",
        ] {
            assert!(create_new(root.path().join(name), b"data".to_vec())
                .await
                .is_err());
        }
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        create_new(root.path().join("snapshot.yml"), b"data".to_vec())
            .await
            .unwrap();
    }
}
