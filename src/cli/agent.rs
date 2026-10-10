//! A local, run-bound JSON-lines adapter. Grants belong to its launcher, never
//! to request data. This is not a sandbox for other programs running as its user.
mod contract;

use std::{
    collections::BTreeMap,
    io::{BufRead, Read},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};

use super::{
    instances::Identity,
    protocol::{self, Request, Response},
};
use crate::core::{owned_paths, readiness::Outcome};
use contract::{Envelope, Failure, Grant, Operation, Reply};

const MAX_LINE: usize = 16 * 1024;
const MAX_OUTPUT: usize = 8 * 1024 * 1024;

#[derive(clap::Args)]
pub(super) struct Args {
    /// Exchange one schema-1 JSON request/response per line on stdin/stdout.
    #[arg(long, required = true)]
    stdio: bool,
    /// Explicit controls available for this session; omitted means read-only.
    #[arg(long, value_enum, value_delimiter = ',')]
    allow: Vec<Grant>,
}

struct Session {
    identity: Identity,
    socket: PathBuf,
    allow: Vec<Grant>,
}

impl Session {
    fn scoped(&self, request: Request) -> Request {
        Request::Scoped {
            instance_id: self.identity.instance_id.clone(),
            run_id: self.identity.run_id.clone(),
            request: Box::new(request),
        }
    }

    fn authorize(&self, operation: &Operation) -> Result<(), Failure> {
        if let Some((grant, target)) = operation.control() {
            if !self.allow.contains(&grant) {
                return Err(Failure::new(
                    "permission-denied",
                    "control was not granted by the Agent session launcher",
                ));
            }
            if target.instance_id != self.identity.instance_id {
                return Err(Failure::new(
                    "wrong-instance",
                    "control targets another instance",
                ));
            }
            if target.run_id != self.identity.run_id {
                return Err(Failure::new(
                    "stale-run",
                    "control targets another run; start a new Agent session",
                ));
            }
        }
        Ok(())
    }

    async fn request(&self, request: Request) -> Result<Response, Failure> {
        let request = self.scoped(request);
        if serde_json::to_vec(&request)
            .map_err(Failure::internal)?
            .len()
            > protocol::MAX_REQUEST
        {
            return Err(Failure::new(
                "invalid-request",
                "request exceeds the supervisor's 4096-byte frame limit",
            ));
        }
        match protocol::request(&self.socket, request)
            .await
            .map_err(Failure::operation)?
        {
            Response::ScopeError { code, message } => Err(Failure { code, message }),
            response => Ok(response),
        }
    }

    async fn execute(&self, operation: Operation) -> Result<Value, Failure> {
        self.authorize(&operation)?;
        let response = match operation {
            Operation::Describe {} => {
                return Ok(json!({
                    "protocol": "devd-agent", "schema_version": 1,
                    "identity": self.identity, "identity_observation": "session-attachment",
                    "allowed_controls": self.allow,
                    "methods": ["describe", "identity", "status", "events", "explain", "wait", "export", "reload-preview", "clean-preview", "restart", "stop", "reload-apply", "clean-apply"],
                    "max_request_bytes": MAX_LINE, "max_response_bytes": MAX_OUTPUT,
                    "scope": "one-instance-and-run", "transport": "json-lines",
                    "request_ids": "correlation-only; controls are not automatically deduplicated",
                    "disconnect": "accepted controls may continue; inspect status/events before retrying"
                }))
            }
            Operation::Identity {} => self.request(Request::Identity).await?,
            Operation::Status {} => self.request(Request::Status).await?,
            Operation::Events { query } => {
                query
                    .validate()
                    .map_err(|e| Failure::new("invalid-request", e))?;
                self.request(Request::Events { query }).await?
            }
            Operation::Explain { service } => self.request(Request::Explain { service }).await?,
            Operation::Export { include_logs } => {
                self.request(Request::Export { include_logs }).await?
            }
            Operation::Wait {
                services,
                timeout_ms,
            } => return self.wait(services, timeout_ms).await,
            Operation::Restart { service, .. } => {
                self.request(Request::Restart {
                    service,
                    expected_run_id: Some(self.identity.run_id.clone()),
                })
                .await?
            }
            Operation::Stop { .. } => {
                self.request(Request::Stop {
                    expected_run_id: Some(self.identity.run_id.clone()),
                })
                .await?
            }
            Operation::ReloadPreview { candidate } => {
                let candidate = self.candidate(candidate).await?;
                self.request(Request::PreviewReload { candidate }).await?
            }
            Operation::ReloadApply {
                candidate, plan_id, ..
            } => {
                super::reload::plan_id(&plan_id).map_err(|e| Failure::new("invalid-request", e))?;
                let candidate = self.candidate(candidate).await?;
                self.request(Request::ApplyReload { candidate, plan_id })
                    .await?
            }
            Operation::CleanPreview {} => return self.clean(None).await,
            Operation::CleanApply { plan_id, .. } => {
                super::reload::plan_id(&plan_id).map_err(|e| Failure::new("invalid-request", e))?;
                return self.clean(Some(plan_id)).await;
            }
        };
        match response {
            Response::Identity(identity) => encode(identity),
            Response::Status(snapshot) => {
                let services: BTreeMap<_, _> = snapshot
                    .services
                    .iter()
                    .map(|(name, state)| (name, super::export::SafeServiceState::from(state)))
                    .collect();
                Ok(json!({"observed_at": chrono::Utc::now(), "services": services}))
            }
            Response::Events(batch) => encode(batch),
            Response::Explain(report) => encode(report),
            Response::Export(report) => encode(report),
            Response::ReloadPlan(plan) => encode(plan),
            Response::Reloaded(report) => encode(report),
            Response::Restarted(state) => encode(super::export::SafeServiceState::from(&state)),
            Response::Stopping => Ok(json!({"outcome": "stopping", "completed": false})),
            _ => Err(Failure::new(
                "invalid-response",
                "unexpected supervisor response",
            )),
        }
    }

    async fn candidate(&self, path: Option<PathBuf>) -> Result<PathBuf, Failure> {
        super::absolute_config(path.as_deref().unwrap_or(&self.identity.config))
            .await
            .map_err(Failure::operation)
    }

    async fn clean(&self, plan: Option<String>) -> Result<Value, Failure> {
        // The core verifies this run under the state lock, including completed
        // receipts, so a stop/start between preview and apply cannot switch scope.
        match super::clean::execute(
            &self.identity.config,
            &self.identity.state_dir,
            self.identity.profile.as_deref(),
            plan,
            Some(self.identity.run_id.clone()),
        )
        .await
        {
            Ok(owned_paths::Output::Preview(plan)) => encode(plan),
            Ok(owned_paths::Output::Applied(report)) => encode(report),
            Err(error) if error.is::<owned_paths::StaleRun>() => {
                Err(Failure::new("stale-run", error.to_string()))
            }
            Err(error) => Err(Failure::operation(error)),
        }
    }

    async fn wait(&self, services: Vec<String>, timeout_ms: u64) -> Result<Value, Failure> {
        if !(1..=3_600_000).contains(&timeout_ms) {
            return Err(Failure::new(
                "invalid-request",
                "timeout_ms must be between 1 and 3600000",
            ));
        }
        let request = self.scoped(Request::Wait {
            services,
            timeout_ms,
        });
        if serde_json::to_vec(&request)
            .map_err(Failure::internal)?
            .len()
            > protocol::MAX_REQUEST
        {
            return Err(Failure::new(
                "invalid-request",
                "wait selection exceeds the supervisor frame limit",
            ));
        }
        let started = tokio::time::Instant::now();
        let mut report = super::wait::Report::empty(Outcome::Unavailable);
        report.instance_id = Some(self.identity.instance_id.clone());
        report.run_id = Some(self.identity.run_id.clone());
        let result = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
            let mut stream = protocol::connect(&self.socket).await?;
            protocol::write(&mut stream, &request).await?;
            super::wait::receive_reports(&mut stream, &mut report).await
        })
        .await;
        match result {
            Err(_) => report.finish(
                Outcome::TimedOut,
                "readiness deadline reached; evidence is the last received observation",
            ),
            Ok(Err(error)) => {
                if let Some(scope) = error.downcast_ref::<protocol::ScopeFailure>() {
                    return Err(Failure::new(&scope.code, &scope.message));
                }
                report.finish(
                    if report.observed_at.is_some() {
                        Outcome::Disconnected
                    } else {
                        Outcome::Unavailable
                    },
                    format!("{error:#}"),
                );
            }
            Ok(Ok(())) => {}
        }
        report.elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        encode(report)
    }
}

fn encode(value: impl serde::Serialize) -> Result<Value, Failure> {
    serde_json::to_value(value).map_err(Failure::internal)
}

/// Blocking stdin belongs to a dedicated thread, not Tokio's blocking pool:
/// cancellation must not hang runtime shutdown waiting for another input byte.
fn input(mut reader: impl BufRead, sender: mpsc::Sender<Result<Vec<u8>, Failure>>) {
    loop {
        let mut line = Vec::new();
        let result = reader
            .by_ref()
            .take((MAX_LINE + 1) as u64)
            .read_until(b'\n', &mut line);
        let message = match result {
            Ok(0) => break,
            Ok(_) if line.len() > MAX_LINE => {
                let _ = sender.blocking_send(Err(Failure::new(
                    "request-too-large",
                    "JSON line exceeds 16384 bytes; session closed",
                )));
                break;
            }
            Ok(_) => Ok(line),
            Err(error) => {
                let _ = sender.blocking_send(Err(Failure::new("input-failed", error.to_string())));
                break;
            }
        };
        if sender.blocking_send(message).is_err() {
            break;
        }
    }
}

async fn serve(
    session: Session,
    mut input: mpsc::Receiver<Result<Vec<u8>, Failure>>,
    output: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    while let Some(line) = input.recv().await {
        let (id, result) = match line.and_then(Envelope::parse) {
            Ok(request) => (Some(request.id), session.execute(request.operation).await),
            Err(error) => (None, Err(error)),
        };
        let mut reply = Reply::new(id, &session.identity, result);
        let mut bytes = serde_json::to_vec(&reply)?;
        if bytes.len() > MAX_OUTPUT {
            reply = Reply::new(
                reply.id,
                &session.identity,
                Err(Failure::new(
                    "response-too-large",
                    "response exceeds 8 MiB; reduce event tail or omit logs",
                )),
            );
            bytes = serde_json::to_vec(&reply)?;
        }
        bytes.push(b'\n');
        tokio::time::timeout(Duration::from_secs(5), async {
            output.write_all(&bytes).await?;
            output.flush().await
        })
        .await
        .context("Agent output stalled; accepted controls may have completed")??;
    }
    Ok(())
}

pub(super) async fn run(
    args: Args,
    socket: &Path,
    config: &Path,
    state: &Path,
    profile: Option<&str>,
) -> Result<()> {
    let mut signals = crate::platform::shutdown::Shutdown::new(false)?;
    let attach = async {
        let Response::Identity(identity) = protocol::request(socket, Request::Identity).await?
        else {
            bail!("supervisor does not support Agent identity attachment");
        };
        let state = tokio::fs::canonicalize(state).await?;
        if identity.schema_version != 1
            || identity.run_id.is_empty()
            || identity.config != config
            || identity.state_dir != state
            || identity.profile.as_deref() != profile
            || identity.instance_id != super::instances::instance_id(config, &state, profile)?
        {
            bail!("supervisor identity does not match the selected instance");
        }
        Ok(identity)
    };
    let identity = tokio::select! {
        _ = signals.recv() => return Ok(()),
        identity = attach => identity?,
    };
    let session = Session {
        identity: *identity,
        socket: socket.into(),
        allow: args.allow,
    };
    let (sender, receiver) = mpsc::channel(1);
    std::thread::Builder::new()
        .name("devd-agent-input".into())
        .spawn(move || input(std::io::stdin().lock(), sender))?;
    let mut output = super::stdout::Stdout::new()?;
    tokio::select! {
        _ = signals.recv() => Ok(()),
        result = serve(session, receiver, &mut output) => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn session() -> Session {
        Session {
            identity: Identity {
                schema_version: 1,
                instance_id: "instance".into(),
                run_id: "run".into(),
                supervisor_pid: 1,
                started_at: chrono::Utc::now(),
                config: "devd.yml".into(),
                state_dir: ".devd".into(),
                profile: None,
                project_root: ".".into(),
                git: None,
            },
            socket: "unused".into(),
            allow: vec![Grant::Restart],
        }
    }

    #[tokio::test]
    async fn test_agent_permission_is_per_operation_and_cannot_be_added_by_a_request() {
        let s = session();
        let target = || contract::Target {
            instance_id: "instance".into(),
            run_id: "run".into(),
        };
        assert!(s
            .authorize(&Operation::Restart {
                service: "api".into(),
                target: target()
            })
            .is_ok());
        for operation in [
            Operation::Stop { target: target() },
            Operation::ReloadApply {
                target: target(),
                candidate: None,
                plan_id: "p".into(),
            },
            Operation::CleanApply {
                target: target(),
                plan_id: "p".into(),
            },
        ] {
            assert_eq!(
                s.execute(operation).await.unwrap_err().code,
                "permission-denied"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_agent_stalled_output_is_bounded_and_eof_drains_requests() {
        let request =
            br#"{"schema_version":1,"id":"d","operation":{"method":"describe"}}"#.to_vec();
        let (sender, receiver) = mpsc::channel(1);
        sender.send(Ok(request.clone())).await.unwrap();
        drop(sender);
        let (mut output, _unread) = tokio::io::duplex(1);
        assert!(serve(session(), receiver, &mut output)
            .await
            .unwrap_err()
            .to_string()
            .contains("stalled"));
        let (sender, receiver) = mpsc::channel(1);
        sender.send(Ok(request)).await.unwrap();
        drop(sender);
        let (mut output, mut read) = tokio::io::duplex(4096);
        serve(session(), receiver, &mut output).await.unwrap();
        drop(output);
        let mut bytes = vec![];
        read.read_to_end(&mut bytes).await.unwrap();
        let reply: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reply["id"], "d");
        assert_eq!(reply["data"]["allowed_controls"], json!(["restart"]));
    }

    #[tokio::test]
    async fn test_agent_wait_timeout_keeps_last_evidence_and_closes_connection() {
        #[cfg(unix)]
        let root = tempfile::tempdir_in("/tmp").unwrap();
        #[cfg(windows)]
        let root = tempfile::tempdir().unwrap();
        let mut s = session();
        s.socket = root.path().join("control.sock");
        let mut listener = super::super::transport::Listener::bind(&s.socket)
            .await
            .unwrap();
        let task = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let request = protocol::read_request(&mut stream).await.unwrap();
            assert!(
                matches!(request, Request::Scoped { ref instance_id, ref run_id, .. } if instance_id == "instance" && run_id == "run")
            );
            let mut report = super::super::wait::Report::empty(Outcome::Waiting);
            report.instance_id = Some("instance".into());
            report.run_id = Some("run".into());
            report.observed_at = Some(chrono::Utc::now());
            report.observation.blocking.push("api".into());
            protocol::write(&mut stream, &Response::Wait(Box::new(report)))
                .await
                .unwrap();
            let mut byte = [0];
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(3), stream.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        let report = s.wait(vec!["api".into()], 500).await.unwrap();
        assert_eq!(report["outcome"], "timed-out");
        assert_eq!(report["blocking"], json!(["api"]));
        assert!(report["observed_at"].is_string());
        task.await.unwrap();
    }
}
