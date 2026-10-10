use super::protocol::{self, Request, Response};
use crate::{
    core::{
        diagnostics,
        events::{query::PersistenceState, storage::EventStorage},
        service_manager::{ManagerOptions, ServiceManager},
        state_store::StateStore,
    },
    logging::{storage::LogStorage, write_logs, LogFormatter},
    storage::StorageOptions,
};
use anyhow::{Context, Result};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::AsyncReadExt,
    sync::{watch, Semaphore},
    task::JoinSet,
};

pub(super) async fn start(
    manager: ServiceManager,
    options: ManagerOptions,
    socket: PathBuf,
    formatter: LogFormatter,
    storage_options: Option<StorageOptions>,
    event_options: Option<StorageOptions>,
    config_path: PathBuf,
) -> Result<()> {
    let mut signals = crate::platform::shutdown::Shutdown::new(false)?;
    // The state lock covers bind, the entire run, and socket cleanup.
    let store = Arc::new(
        StateStore::open(&options.state_path)
            .await
            .context("cannot own project state; another supervisor may already be running")?,
    );
    let mut listener = super::transport::Listener::bind(&socket).await?;
    let snapshots = manager.subscribe();
    let history = manager.log_history();
    let event_history = manager.event_history();
    let (event_status, event_persistence) = watch::channel(if event_options.is_some() {
        PersistenceState::Recording
    } else {
        PersistenceState::Disabled
    });
    let controller = manager.controller();
    let configurations = manager.configurations();
    let readiness = manager.readiness();
    let waiters = Arc::new(Semaphore::new(8));
    let profile = options.profile.clone();
    let previews = Arc::new(Semaphore::new(2));
    let logs = manager.subscribe_logs();
    // Open under the state lock, before polling the manager and spawning any
    // services. Each blocking writer owns its disk lock until drain completes.
    let storage = if let Some(options) = storage_options {
        let directory = socket.parent().unwrap().join("logs");
        let lease = store.clone();
        Some(
            tokio::task::spawn_blocking(move || {
                let _lease = lease;
                LogStorage::open(&directory, options)
            })
            .await
            .context("disk log setup task failed")?
            .context("cannot open persistent logs")?,
        )
    } else {
        None
    };
    let disk_logs = storage.as_ref().map(|_| manager.subscribe_logs());
    let event_storage = if let Some(options) = event_options {
        let directory = socket.parent().unwrap().join("events");
        let context = event_history.snapshot().context;
        let lease = store.clone();
        Some(
            tokio::task::spawn_blocking(move || {
                let _lease = lease;
                EventStorage::open(&directory, options, context)
            })
            .await
            .context("disk event setup task failed")?
            .context("cannot open persistent events")?,
        )
    } else {
        None
    };
    let disk_events = event_storage.as_ref().map(|_| manager.subscribe_events());
    let identity = super::instances::register(
        config_path,
        socket
            .parent()
            .context("control endpoint has no directory")?,
        profile.clone(),
        event_history.snapshot().context.run_id,
        store.clone(),
    )
    .await
    .context("cannot register project instance")?;
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
    let mut disk_writer = JoinSet::new();
    if let Some((storage, logs)) = storage.zip(disk_logs) {
        let lease = store.clone();
        disk_writer.spawn_blocking(move || {
            let _lease = lease;
            storage.run(logs)
        });
    }
    let mut writer = JoinSet::new();
    let mut event_writer = JoinSet::new();
    if let Some((storage, events)) = event_storage.zip(disk_events) {
        let lease = store.clone();
        event_writer.spawn_blocking(move || {
            let _lease = lease;
            storage.run(events)
        });
    }
    writer.spawn(write_logs(stdout, logs, formatter));
    let mut writer_done = false;
    let mut clients = JoinSet::new();
    // Long-lived followers must leave room for status/stop/restart requests.
    let followers = Arc::new(Semaphore::new(16));
    let mut failure = None;
    let mut diagnostics = JoinSet::new();
    let result = loop {
        tokio::select! {
            biased;
            result = &mut run => break result,
            _ = signals.recv() => { shutdown.send_replace(true); },
            Some(result) = event_writer.join_next() => {
                if let Err(error) = result.context("disk event writer task failed").and_then(|r| r.context("cannot write persistent events")) {
                    event_status.send_replace(PersistenceState::Failed);
                    diagnostics.spawn(report_event_failure(format!("{error:#}; disk event recording disabled for this run; services continue\n")));
                }
            },
            Some(result) = disk_writer.join_next() => {
                if let Err(error) = result.context("disk log writer task failed").and_then(|r| r.context("cannot write persistent logs")) {
                    failure.get_or_insert(error);
                    shutdown.send_replace(true);
                }
            },
            Some(result) = writer.join_next(), if !writer_done => {
                writer_done = true;
                if let Err(error) = result.context("log writer task failed").and_then(|r| r.context("cannot write service logs")) {
                    failure.get_or_insert(error);
                    shutdown.send_replace(true);
                }
            },
            Some(_) = clients.join_next() => {},
            Some(_) = diagnostics.join_next() => {},
            accepted = listener.accept(), if clients.len() < 32 && !*shutdown.borrow() => {
                let mut stream = match accepted {
                    Ok(client) => client,
                    Err(error) => { failure.get_or_insert(error.into()); shutdown.send_replace(true); continue; }
                };
                let snapshots = snapshots.clone();
                let history = history.clone();
                let event_history = event_history.clone();
                let event_persistence = event_persistence.clone();
                let controller = controller.clone();
                let shutdown = shutdown.clone();
                let followers = followers.clone();
                let previews = previews.clone();
                let configurations = configurations.clone();
                let profile = profile.clone();
                let identity = identity.clone();
                let readiness = readiness.clone();
                let waiters = waiters.clone();
                clients.spawn(async move {
                    // Reload can remove a service while its bounded diagnostic
                    // history remains useful. Control still requires a live name.
                    let known_service = |name: &str| snapshots.borrow().services.contains_key(name)
                        || !history.recent(Some(name), 1).is_empty()
                        || event_history.snapshot().entries.iter().any(|event| event.service.as_deref() == Some(name));
                    let request = match protocol::read_request(&mut stream).await {
                        Ok(Request::Scoped { instance_id, run_id, request }) => {
                            let failure = if instance_id != identity.instance_id {
                                Some(("wrong-instance", "supervisor instance changed"))
                            } else if run_id != identity.run_id {
                                Some(("stale-run", "supervisor run changed; start a new Agent session"))
                            } else if matches!(*request, Request::Scoped { .. }) {
                                Some(("invalid-request", "nested scope envelopes are not supported"))
                            } else { None };
                            if let Some((code, message)) = failure {
                                return protocol::write(&mut stream, &Response::ScopeError { code: code.into(), message: message.into() }).await;
                            }
                            Ok(*request)
                        },
                        other => other,
                    };
                    let response = match request {
                        Ok(Request::Scoped { .. }) => unreachable!("scoped envelope already checked"),
                        Err(error) => Response::Error(error.to_string()),
                        Ok(request) if request.expected_run_id().is_some_and(|run| run != identity.run_id) => {
                            Response::Error("supervisor run changed; reopen top before controlling this instance".into())
                        },
                        Ok(Request::Status) => Response::Status(snapshots.borrow().clone()),
                        Ok(Request::Identity) => Response::Identity(Box::new(identity)),
                        Ok(Request::Wait { services, timeout_ms }) => {
                            let permit = waiters.try_acquire_owned().ok();
                            return super::wait::serve(&mut stream, services, timeout_ms, super::wait::Source {
                                identity, snapshots, configurations, control: readiness, stopping: shutdown.subscribe(),
                            }, permit).await;
                        },
                        Ok(Request::PreviewReload { candidate }) => {
                            let Ok(permit) = previews.try_acquire_owned() else {
                                return protocol::write(&mut stream, &Response::Error("too many configuration previews (maximum 2)".into())).await;
                            };
                            let mut stopping = shutdown.subscribe();
                            if *stopping.borrow() {
                                Response::Error("supervisor is stopping".into())
                            } else {
                                tokio::select! {
                                    biased;
                                    _ = stopping.changed() => Response::Error("supervisor is stopping".into()),
                                    result = super::reload::preview(candidate, configurations, profile, snapshots, permit) => match result {
                                        Ok(plan) => Response::ReloadPlan(plan),
                                        Err(error) => Response::Error(format!("cannot preview configuration: {error:#}")),
                                    },
                                }
                            }
                        }
                        Ok(Request::ApplyReload { candidate, plan_id }) => {
                            if let Err(error) = super::reload::plan_id(&plan_id) {
                                return protocol::write(&mut stream, &Response::Error(error)).await;
                            }
                            let Ok(permit) = previews.try_acquire_owned() else {
                                return protocol::write(&mut stream, &Response::Error("too many configuration reads (maximum 2)".into())).await;
                            };
                            let mut stopping = shutdown.subscribe();
                            if *stopping.borrow() { Response::Error("supervisor is stopping".into()) }
                            else {
                                let candidate = tokio::select! {
                                    biased;
                                    _ = stopping.changed() => return protocol::write(&mut stream, &Response::Error("supervisor is stopping".into())).await,
                                    result = super::reload::load_candidate(candidate, profile, permit) => result,
                                };
                                match candidate {
                                    Err(error) => Response::Error(format!("cannot read candidate configuration: {error:#}")),
                                    Ok(config) => match controller.reload(config, plan_id).await {
                                        Ok(report) => Response::Reloaded(report),
                                        Err(error) => Response::Error(error),
                                    },
                                }
                            }
                        }
                        Ok(Request::Explain { service }) => {
                            if !known_service(&service) {
                                Response::Error(format!("unknown service '{service}'"))
                            } else {
                                let query = crate::core::events::query::EventQuery {
                                    filter: crate::core::events::query::EventFilter::default(),
                                    tail: 1000,
                                    cursor: None,
                                };
                                match query.select(event_history.snapshot()) {
                                    Ok(mut batch) => {
                                        batch.persistence = Some(*event_persistence.borrow());
                                        Response::Explain(diagnostics::explain(
                                            &service,
                                            Some(&snapshots.borrow()),
                                            &batch,
                                        ))
                                    }
                                    Err(error) => Response::Error(error),
                                }
                            }
                        }
                        Ok(Request::Export { include_logs }) => {
                            match super::export::capture(
                                identity,
                                &snapshots,
                                &event_history,
                                *event_persistence.borrow(),
                                include_logs.then_some(&history),
                            ) {
                                Ok(report) => Response::Export(Box::new(report)),
                                Err(error) => Response::Error(error),
                            }
                        }
                        Ok(request @ (Request::Events { .. } | Request::FollowEvents { .. })) => {
                            let follow = matches!(request, Request::FollowEvents { .. });
                            let (Request::Events { query } | Request::FollowEvents { query }) = request else { unreachable!() };
                            if let Err(error) = query.validate() {
                                return protocol::write(&mut stream, &Response::Error(error)).await;
                            }
                            if let Some(name) = &query.filter.service {
                                if !known_service(name) {
                                    return protocol::write(&mut stream, &Response::Error(format!("unknown service '{name}'"))).await;
                                }
                            }
                            if follow {
                                let Ok(_permit) = followers.try_acquire_owned() else {
                                    return protocol::write(&mut stream, &Response::Error("too many followers (maximum 16)".into())).await;
                                };
                                if let Err(error) = super::events::follow(&mut stream, event_history, query, event_persistence).await {
                                    protocol::write(&mut stream, &Response::Error(error.to_string())).await?;
                                }
                                return Ok(());
                            }
                            match query.select(event_history.snapshot()) {
                                Ok(mut batch) => { batch.persistence = Some(*event_persistence.borrow()); Response::Events(batch) }
                                Err(error) => Response::Error(error),
                            }
                        }
                        Ok(Request::Stop { .. }) => {
                            let result = protocol::write(&mut stream, &Response::Stopping).await;
                            shutdown.send_replace(true);
                            return result;
                        }
                        Ok(Request::Logs { service, tail, filter }) => {
                            if service.as_ref().is_some_and(|name| !known_service(name)) {
                                Response::Error(format!("unknown service '{}'", service.unwrap()))
                            } else if !(1..=1000).contains(&tail) {
                                Response::Error("tail must be between 1 and 1000".into())
                            } else {
                                Response::Logs(history.recent_filtered(service.as_deref(), &filter, tail).iter().map(|entry| (**entry).clone()).collect())
                            }
                        }
                        Ok(Request::FollowLogs { service, tail, filter, .. }) => {
                            if service.as_ref().is_some_and(|name| !known_service(name)) {
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
                        Ok(Request::Restart { service, .. }) => {
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
    if let Some(result) = event_writer.join_next().await {
        if let Err(error) = result
            .context("disk event writer task failed")
            .and_then(|r| r.context("cannot write persistent events"))
        {
            event_status.send_replace(PersistenceState::Failed);
            diagnostics.spawn(report_event_failure(format!(
                "{error:#}; disk event recording incomplete\n"
            )));
        }
    }
    // Disk persistence is not subject to the terminal writer's one-second
    // timeout: drain all accepted entries and sync before releasing state.
    if let Some(result) = disk_writer.join_next().await {
        if let Err(error) = result
            .context("disk log writer task failed")
            .and_then(|r| r.context("cannot write persistent logs"))
        {
            failure.get_or_insert(error);
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while clients.join_next().await.is_some() {}
    })
    .await;
    clients.shutdown().await;
    while diagnostics.join_next().await.is_some() {}
    if !writer_done {
        match tokio::time::timeout(Duration::from_secs(1), writer.join_next()).await {
            Ok(Some(result)) => {
                if let Err(error) = result
                    .context("log writer task failed")
                    .and_then(|r| r.context("cannot write service logs"))
                {
                    failure.get_or_insert(error);
                }
            }
            Err(_) => {
                writer.shutdown().await;
            }
            Ok(None) => {}
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    result?;
    Ok(())
}

async fn report_event_failure(message: String) {
    use tokio::io::AsyncWriteExt;
    // A closed or stalled diagnostic pipe must not change event-storage's
    // nonfatal policy or keep the supervisor from serving control requests.
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        let mut stderr = super::stdout::Stdout::stderr()?;
        stderr.write_all(message.as_bytes()).await?;
        stderr.flush().await
    })
    .await;
}
