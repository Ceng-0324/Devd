//! Windows console and inherited synchronous pipe handles cannot be registered
//! with Tokio's IOCP. Keep one bounded write on a dedicated, cancellable thread;
//! never use the runtime's blocking pool, which is joined at runtime shutdown.
use std::{
    fs::File,
    future::Future,
    io::{self, IsTerminal, Write},
    os::windows::io::{AsHandle, AsRawHandle},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    task::{Context, Poll},
    thread::JoinHandle,
};
use tokio::{io::AsyncWrite, sync::oneshot};

struct WriteRequest {
    bytes: Vec<u8>,
    done: oneshot::Sender<io::Result<usize>>,
}

pub(super) struct Stdout {
    sender: Option<mpsc::SyncSender<WriteRequest>>,
    pending: Option<oneshot::Receiver<io::Result<usize>>>,
    cancelled: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Stdout {
    pub(super) fn new() -> io::Result<Self> {
        // Avoid std's global stdout lock: process-exit flushing could otherwise
        // wait for a worker blocked on a full redirected pipe.
        let console = io::stdout().is_terminal();
        let mut output = File::from(io::stdout().as_handle().try_clone_to_owned()?);
        let (sender, receiver) = mpsc::sync_channel::<WriteRequest>(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let stopped = cancelled.clone();
        let thread = std::thread::Builder::new()
            .name("devd-stdout".into())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let result = if request.bytes.is_empty() {
                        output.flush().map(|_| 0)
                    } else if console {
                        write_console(&output, &request.bytes)
                    } else {
                        output.write(&request.bytes)
                    };
                    let _ = request.done.send(result);
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            pending: None,
            cancelled,
            thread,
        })
    }

    fn poll_operation(&mut self, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        if self.pending.is_none() {
            let (done, receiver) = oneshot::channel();
            let mut length = bytes.len().min(16 * 1024);
            // LogFormatter supplies UTF-8. Do not split a console code point.
            if let Ok(text) = std::str::from_utf8(bytes) {
                while !text.is_char_boundary(length) {
                    length -= 1;
                }
            }
            let request = WriteRequest {
                bytes: bytes[..length].to_vec(),
                done,
            };
            if self.sender.as_ref().unwrap().try_send(request).is_err() {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "stdout worker stopped",
                )));
            }
            self.pending = Some(receiver);
        }
        let result = std::task::ready!(Pin::new(self.pending.as_mut().unwrap()).poll(cx));
        self.pending = None;
        Poll::Ready(result.unwrap_or_else(|_| {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stdout worker stopped",
            ))
        }))
    }
}

fn write_console(file: &File, bytes: &[u8]) -> io::Result<usize> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut written = 0;
    // SAFETY: the handle is a live console; units and written outlive the call.
    if unsafe {
        windows_sys::Win32::System::Console::WriteConsoleW(
            file.as_raw_handle(),
            units.as_ptr().cast(),
            units.len() as u32,
            &mut written,
            std::ptr::null(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if written as usize != units.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "partial Windows console write",
        ));
    }
    Ok(bytes.len())
}

impl AsyncWrite for Stdout {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_operation(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_operation(cx, &[])
            .map(|result| result.map(|_| ()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl Drop for Stdout {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.sender.take();
        // Best effort: cancellation can race with a synchronous write starting.
        // A detached worker cannot hold Tokio shutdown hostage in either case.
        unsafe {
            windows_sys::Win32::System::IO::CancelSynchronousIo(self.thread.as_raw_handle());
        }
    }
}
