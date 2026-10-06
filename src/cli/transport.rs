//! Local-only control transport. The state lock must be held while binding.
use std::{io, path::Path};
use tokio::io::{AsyncRead, AsyncWrite};

pub(super) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub(super) type Stream = Box<dyn Io>;

#[cfg(unix)]
pub(super) struct Listener {
    listener: tokio::net::UnixListener,
    path: std::path::PathBuf,
}

#[cfg(unix)]
impl Listener {
    pub(super) async fn bind(path: &Path) -> anyhow::Result<Self> {
        use anyhow::{bail, Context};
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) if metadata.file_type().is_socket() => {
                tokio::fs::remove_file(path).await?
            }
            Ok(_) => bail!("refusing to replace non-socket path {}", path.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = tokio::net::UnixListener::bind(path).with_context(|| {
            format!(
                "cannot bind {}; try a shorter --state-dir path",
                path.display()
            )
        })?;
        let listener = Self {
            listener,
            path: path.into(),
        };
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
        Ok(listener)
    }

    pub(super) async fn accept(&mut self) -> io::Result<Stream> {
        self.listener
            .accept()
            .await
            .map(|(stream, _)| Box::new(stream) as Stream)
    }
}

#[cfg(unix)]
impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
pub(super) async fn connect(path: &Path) -> io::Result<Stream> {
    Ok(Box::new(tokio::net::UnixStream::connect(path).await?))
}

#[cfg(windows)]
pub(super) struct Listener {
    pending: tokio::net::windows::named_pipe::NamedPipeServer,
    name: String,
    security: crate::platform::security::Descriptor,
}

#[cfg(windows)]
impl Listener {
    pub(super) async fn bind(path: &Path) -> anyhow::Result<Self> {
        let name = pipe_name(path)?;
        let security = crate::platform::security::Descriptor::current_user()?;
        let pending = security.pipe(&name, true)?;
        Ok(Self {
            pending,
            name,
            security,
        })
    }

    pub(super) async fn accept(&mut self) -> io::Result<Stream> {
        self.pending.connect().await?;
        // Keep at least one instance alive continuously; never release ownership
        // of the pipe name between clients. Connect is cancellation-safe.
        let next = self.security.pipe(&self.name, false)?;
        Ok(Box::new(std::mem::replace(&mut self.pending, next)))
    }
}

#[cfg(windows)]
fn pipe_name(path: &Path) -> io::Result<String> {
    use std::os::windows::ffi::OsStrExt;
    let path = path
        .parent()
        .unwrap_or(Path::new("."))
        .canonicalize()?
        .join(
            path.file_name()
                .ok_or_else(|| io::Error::other("missing endpoint name"))?,
        );
    // Stable FNV-1a over canonical UTF-16 avoids the named-pipe path length limit.
    let mut hash = 0x6c62272e07bb014262b821756295c58du128;
    for unit in path.as_os_str().encode_wide() {
        for byte in unit.to_le_bytes() {
            hash ^= u128::from(byte);
            hash = hash.wrapping_mul(0x0000000001000000000000000000013b);
        }
    }
    Ok(format!(r"\\.\pipe\devd-{hash:032x}"))
}

#[cfg(windows)]
pub(super) async fn connect(path: &Path) -> io::Result<Stream> {
    use tokio::net::windows::named_pipe::ClientOptions;
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
    let name = pipe_name(path)?;
    loop {
        match ClientOptions::new().open(&name) {
            Ok(client) => return Ok(Box::new(client)),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn test_transport_reconnects_and_releases_endpoint() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("control.sock");
        let mut listener = Listener::bind(&path).await.unwrap();
        for _ in 0..3 {
            let (server, client) = tokio::join!(listener.accept(), connect(&path));
            let mut server = server.unwrap();
            let mut client = client.unwrap();
            client.write_all(b"request").await.unwrap();
            let mut bytes = [0; 7];
            server.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"request");
        }
        drop(listener);
        assert!(connect(&path).await.is_err());
        assert!(Listener::bind(&path).await.is_ok());
    }
}
