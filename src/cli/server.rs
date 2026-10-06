use super::protocol::{self, Request, Response};
use crate::{
    core::{
        service_manager::{ManagerOptions, ServiceManager},
        state_store::StateStore,
    },
    logging::{write_logs, LogFormatter},
};
use anyhow::{bail, Context, Result};
use std::{
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    net::UnixListener,
    signal::unix::{signal, SignalKind},
    sync::{watch, Semaphore},
    task::JoinSet,
};

struct Endpoint(PathBuf);
impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub(super) async fn start(
    manager: ServiceManager,
    options: ManagerOptions,
    socket: PathBuf,
    formatter: LogFormatter,
) -> Result<()> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    // The state lock covers bind, the entire run, and socket cleanup.
    let store = Arc::new(
        StateStore::open(&options.state_path)
            .await
            .context("cannot own project state; another supervisor may already be running")?,
    );
    match tokio::fs::symlink_metadata(&socket).await {
        Ok(metadata) if metadata.file_type().is_socket() => tokio::fs::remove_file(&socket).await?,
        Ok(_) => bail!("refusing to replace non-socket path {}", socket.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(&socket).with_context(|| {
        format!(
            "cannot bind {}; try a shorter --state-dir path",
            socket.display()
        )
    })?;
    let _endpoint = Endpoint(socket.clone());
    tokio::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).await?;
    let snapshots = manager.subscribe();
    let history = manager.log_history();
    let controller = manager.controller();
    let logs = manager.subscribe_logs();
    let (shutdown, mut stopping) = watch::channel(false);
    let mut run = Box::pin(manager.run_with_store(
        async move {
            while !*stopping.borrow_and_update() {
                if stopping.changed().await.is_err() {
                    break;
                }
            }
        },
        store.clone(),
    ));
    let stdout = super::stdout::Stdout::new()?;
    let mut writer = JoinSet::new();
    writer.spawn(write_logs(stdout, logs, formatter));
    let mut writer_done = false;
    let mut clients = JoinSet::new();
    // Long-lived followers must leave room for status/stop/restart requests.
    let followers = Arc::new(Semaphore::new(16));
    let mut failure = None;
    let result = loop {
        tokio::select! {
            biased;
            result = &mut run => break result,
            _ = interrupt.recv() => { shutdown.send_replace(true); },
            _ = terminate.recv() => { shutdown.send_replace(true); },
            Some(result) = writer.join_next(), if !writer_done => {
                writer_done = true;
                if let Err(error) = result.context("log writer task failed").and_then(|r| r.context("cannot write service logs")) {
                    failure = Some(error);
                    shutdown.send_replace(true);
                }
            },
            Some(_) = clients.join_next() => {},
            accepted = listener.accept(), if clients.len() < 32 && !*shutdown.borrow() => {
                let (mut stream, _) = match accepted {
                    Ok(client) => client,
                    Err(error) => { failure = Some(error.into()); shutdown.send_replace(true); continue; }
                };
                let snapshots = snapshots.clone();
                let history = history.clone();
                let controller = controller.clone();
                let shutdown = shutdown.clone();
                let followers = followers.clone();
                clients.spawn(async move {
                    let response = match protocol::read_request(&mut stream).await {
                        Err(error) => Response::Error(error.to_string()),
                        Ok(Request::Status) => Response::Status(snapshots.borrow().clone()),
                        Ok(Request::Stop) => {
                            let result = protocol::write(&mut stream, &Response::Stopping).await;
                            shutdown.send_replace(true);
                            return result;
                        }
                        Ok(Request::Logs { service, tail, filter }) => {
                            if service.as_ref().is_some_and(|name| !snapshots.borrow().services.contains_key(name)) {
                                Response::Error(format!("unknown service '{}'", service.unwrap()))
                            } else if !(1..=1000).contains(&tail) {
                                Response::Error("tail must be between 1 and 1000".into())
                            } else {
                                Response::Logs(history.recent_filtered(service.as_deref(), &filter, tail).iter().map(|entry| (**entry).clone()).collect())
                            }
                        }
                        Ok(Request::FollowLogs { service, tail, filter }) => {
                            if service.as_ref().is_some_and(|name| !snapshots.borrow().services.contains_key(name)) {
                                return protocol::write(&mut stream, &Response::Error(format!("unknown service '{}'", service.unwrap()))).await;
                            }
                            if !(1..=1000).contains(&tail) {
                                return protocol::write(&mut stream, &Response::Error("tail must be between 1 and 1000".into())).await;
                            }
                            let Ok(_permit) = followers.try_acquire_owned() else {
                                return protocol::write(&mut stream, &Response::Error("too many log followers (maximum 16)".into())).await;
                            };
                            let (entries, mut live) = history.subscribe_with_filtered(service.as_deref(), &filter, tail);
                            protocol::write(&mut stream, &Response::Logs(entries.iter().map(|entry| (**entry).clone()).collect())).await?;
                            loop {
                                let mut unexpected = [0];
                                let entry = tokio::select! {
                                    result = stream.read(&mut unexpected) => {
                                        if result? == 0 { return Ok(()); }
                                        return protocol::write(&mut stream, &Response::Error("unexpected input after log subscription".into())).await;
                                    }
                                    entry = live.recv() => entry,
                                };
                                match entry {
                                    Ok(entry) if service.as_ref().is_none_or(|name| entry.service == *name) && filter.matches(&entry) => {
                                        protocol::write(&mut stream, &Response::Log((*entry).clone())).await?;
                                    }
                                    Ok(_) => {}
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                                        return protocol::write(&mut stream, &Response::Error(format!("log stream skipped {count} entries; reconnect to inspect recent history"))).await;
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                                }
                            }
                        }
                        Ok(Request::Restart { service }) => {
                            match tokio::time::timeout(Duration::from_secs(55), controller.restart(service)).await {
                                Ok(Ok(state)) => Response::Restarted(state),
                                Ok(Err(error)) => Response::Error(error),
                                Err(_) => Response::Error("restart is still pending; inspect 'devd status' before retrying".into()),
                            }
                        }
                    };
                    protocol::write(&mut stream, &response).await
                });
            }
        }
    };
    drop(run);
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while clients.join_next().await.is_some() {}
    })
    .await;
    clients.shutdown().await;
    if !writer_done {
        match tokio::time::timeout(Duration::from_secs(1), writer.join_next()).await {
            Ok(Some(result)) => {
                result.context("log writer task failed")??;
            }
            Err(_) => {
                writer.shutdown().await;
            }
            Ok(None) => {}
        }
    }
    result?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}
