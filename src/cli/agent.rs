//! A local, run-bound JSON-lines adapter. Grants belong to its launcher, never
//! to request data. This is not a sandbox for other programs running as its user.
mod contract;

use std::{
    io::{BufRead, Read},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};

use super::control_session::{attach, Failure, Grant, Session};
use contract::{Envelope, Reply};

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
    /// Limit accepted restarts per service for this adapter session (repeat SERVICE=COUNT).
    #[arg(long = "restart-budget", value_name = "SERVICE=COUNT")]
    restart_budget: Vec<String>,
    /// Deduplicate control requests for 10 minutes within this adapter session.
    #[arg(long)]
    deduplicate: bool,
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
    let policy = super::control_session::parse_policy(&args.restart_budget, args.deduplicate)?;
    let mut signals = crate::platform::shutdown::Shutdown::new(false)?;
    let session = tokio::select! {
        _ = signals.recv() => return Ok(()),
        identity = attach(socket, config, state, profile, args.allow, policy) => identity?,
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
    use super::super::control_session::{Operation, Target};
    use super::super::{
        instances::Identity,
        protocol::{self, Request, Response},
    };
    use super::*;
    use crate::core::readiness::Outcome;
    use serde_json::json;
    use serde_json::Value;
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
            policy: Default::default(),
            admission: Default::default(),
            controls: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    #[tokio::test]
    async fn test_agent_permission_is_per_operation_and_cannot_be_added_by_a_request() {
        let s = session();
        let target = || Target {
            instance_id: "instance".into(),
            run_id: "run".into(),
        };
        assert!(s
            .authorize(&Operation::Restart {
                service: "api".into(),
                target: target(),
                operation_key: None,
            })
            .is_ok());
        for operation in [
            Operation::Stop {
                target: target(),
                operation_key: None,
            },
            Operation::ReloadApply {
                target: target(),
                candidate: None,
                plan_id: "p".into(),
                operation_key: None,
            },
            Operation::CleanApply {
                target: target(),
                plan_id: "p".into(),
                operation_key: None,
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
