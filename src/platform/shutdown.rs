use std::io;

/// Register before spawning children, so an early interrupt cannot leave them
/// running. TUI additionally treats SIGHUP as an instruction to restore its tty.
pub(crate) struct Shutdown {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hangup: Option<tokio::signal::unix::Signal>,
    #[cfg(windows)]
    interrupt: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    terminate: tokio::signal::windows::CtrlBreak,
}

impl Shutdown {
    pub(crate) fn new(include_hangup: bool) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())?,
                terminate: signal(SignalKind::terminate())?,
                hangup: include_hangup
                    .then(|| signal(SignalKind::hangup()))
                    .transpose()?,
            })
        }
        #[cfg(windows)]
        {
            let _ = include_hangup;
            Ok(Self {
                interrupt: tokio::signal::windows::ctrl_c()?,
                terminate: tokio::signal::windows::ctrl_break()?,
            })
        }
    }

    pub(crate) async fn recv(&mut self) {
        #[cfg(unix)]
        let hangup = async {
            match &mut self.hangup {
                Some(signal) => {
                    signal.recv().await;
                }
                None => std::future::pending().await,
            }
        };
        #[cfg(windows)]
        let hangup = std::future::pending::<()>();
        tokio::select! {
            _ = self.interrupt.recv() => {},
            _ = self.terminate.recv() => {},
            _ = hangup => {},
        }
    }
}
