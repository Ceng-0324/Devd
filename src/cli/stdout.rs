//! Nonblocking terminal/pipe output lets shutdown cancel a stalled consumer.
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use std::{
    fs::File,
    io::{self, Write},
    os::fd::AsFd,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    fs,
    io::{unix::AsyncFd, AsyncWrite},
};

pub(super) enum Stdout {
    File(fs::File),
    Pollable { file: AsyncFd<File>, flags: OFlag },
}

impl Stdout {
    pub fn new() -> io::Result<Self> {
        let file = File::from(io::stdout().as_fd().try_clone_to_owned()?);
        if file.metadata()?.is_file() {
            return Ok(Self::File(fs::File::from_std(file)));
        }
        let flags = OFlag::from_bits_truncate(fcntl(&file, FcntlArg::F_GETFL)?);
        let file = match AsyncFd::new(file.try_clone()?) {
            Ok(pollable) => pollable,
            // epoll/kqueue reject devices such as /dev/null with EPERM/EINVAL.
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(nix::libc::EPERM | nix::libc::EINVAL | nix::libc::ENODEV)
                ) =>
            {
                return Ok(Self::File(fs::File::from_std(file)));
            }
            Err(error) => return Err(error),
        };
        fcntl(file.get_ref(), FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
        Ok(Self::Pollable { file, flags })
    }
}

impl Drop for Stdout {
    fn drop(&mut self) {
        if let Self::Pollable { file, flags } = self {
            let _ = fcntl(file.get_ref(), FcntlArg::F_SETFL(*flags));
        }
    }
}

impl AsyncWrite for Stdout {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Self::File(file) => Pin::new(file).poll_write(cx, bytes),
            Self::Pollable { file, .. } => loop {
                let mut ready = std::task::ready!(file.poll_write_ready(cx))?;
                match ready.try_io(|file| file.get_ref().write(bytes)) {
                    Ok(result) => return Poll::Ready(result),
                    Err(_) => continue,
                }
            },
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::File(file) => Pin::new(file).poll_flush(cx),
            Self::Pollable { .. } => Poll::Ready(Ok(())),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}
