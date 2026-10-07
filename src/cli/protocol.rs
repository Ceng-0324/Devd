use std::{path::Path, time::Duration};

use super::transport::{self, Stream};
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{
    core::events::query::{EventBatch, EventQuery},
    core::service_manager::{RuntimeSnapshot, ServiceSnapshot},
    logging::{LogEntry, LogFilter},
};

const MAX_REQUEST: usize = 4096;
const MAX_RESPONSE: usize = 128 * 1024 * 1024;
pub(super) const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum Request {
    Status,
    Stop,
    Events {
        query: EventQuery,
    },
    FollowEvents {
        query: EventQuery,
    },
    Restart {
        service: String,
    },
    Logs {
        service: Option<String>,
        tail: usize,
        #[serde(default)]
        filter: LogFilter,
    },
    FollowLogs {
        service: Option<String>,
        tail: usize,
        #[serde(default)]
        filter: LogFilter,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "result", content = "data", rename_all = "kebab-case")]
pub(super) enum Response {
    Status(RuntimeSnapshot),
    Stopping,
    Events(EventBatch),
    Restarted(ServiceSnapshot),
    Logs(Vec<LogEntry>),
    Log(LogEntry),
    Error(String),
}

async fn read<T: DeserializeOwned>(
    stream: &mut (impl AsyncRead + Unpin),
    maximum: usize,
) -> Result<T> {
    let length = stream.read_u32().await? as usize;
    if length > maximum {
        bail!("control message exceeds {maximum} bytes");
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).context("invalid control message")
}

pub(super) async fn read_request(stream: &mut (impl AsyncRead + Unpin)) -> Result<Request> {
    tokio::time::timeout(IO_TIMEOUT, read(stream, MAX_REQUEST))
        .await
        .context("control request timed out")?
}

pub(super) async fn write<T: Serialize>(
    stream: &mut (impl AsyncWrite + Unpin),
    message: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    if bytes.len() > MAX_RESPONSE {
        bail!("control response is too large");
    }
    tokio::time::timeout(IO_TIMEOUT, async {
        stream.write_u32(bytes.len() as u32).await?;
        stream.write_all(&bytes).await
    })
    .await
    .context("control write timed out")??;
    Ok(())
}

pub(super) async fn request(socket: &Path, message: Request) -> Result<Response> {
    let mut stream = connect(socket).await?;
    write(&mut stream, &message).await?;
    let response = tokio::time::timeout(Duration::from_secs(60), read(&mut stream, MAX_RESPONSE))
        .await
        .context("supervisor response timed out; inspect 'devd status' before retrying")??;
    match response {
        Response::Error(error) => bail!("{error}"),
        other => Ok(other),
    }
}

pub(super) async fn connect(socket: &Path) -> Result<Stream> {
    let stream = tokio::time::timeout(IO_TIMEOUT, transport::connect(socket)).await?
        .with_context(|| format!("no reachable devd supervisor at {}; run 'devd start' with the same --config and --state-dir", socket.display()))?;
    Ok(stream)
}

pub(super) async fn next_response(
    stream: &mut (impl AsyncRead + Unpin),
) -> Result<Option<Response>> {
    let mut prefix = [0u8; 1];
    if stream.read(&mut prefix).await? == 0 {
        return Ok(None);
    }
    // Silence between frames is valid; a partially transmitted frame is not.
    tokio::time::timeout(IO_TIMEOUT, async {
        let mut rest = [0u8; 3];
        stream.read_exact(&mut rest).await?;
        let length = u32::from_be_bytes([prefix[0], rest[0], rest[1], rest[2]]) as usize;
        if length > MAX_RESPONSE {
            bail!("control message exceeds {MAX_RESPONSE} bytes");
        }
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes).await?;
        let response = serde_json::from_slice(&bytes).context("invalid control message")?;
        Ok(Some(response))
    })
    .await
    .context("control stream frame timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_protocol_log_filter_defaults_and_rejects_unknown_fields() {
        let old: Request =
            serde_json::from_str(r#"{"command":"logs","service":null,"tail":10}"#).unwrap();
        assert!(matches!(
            old,
            Request::Logs {
                filter: LogFilter {
                    level: None,
                    since: None,
                    grep: None
                },
                ..
            }
        ));

        let filtered: Request = serde_json::from_str(
            r#"{"command":"follow-logs","service":"api","tail":3,"filter":{"level":"Error","grep":"database"}}"#,
        )
        .unwrap();
        assert!(matches!(
            filtered,
            Request::FollowLogs {
                filter: LogFilter {
                    level: Some(crate::logging::LogLevel::Error),
                    grep: Some(_),
                    ..
                },
                ..
            }
        ));

        assert!(serde_json::from_str::<Request>(
            r#"{"command":"logs","service":null,"tail":10,"filter":{"regex":".*"}}"#
        )
        .is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn test_cli_log_stream_bounds_partial_frames_but_allows_silence() {
        for partial in [vec![0], vec![0, 0, 0, 100, b'{']] {
            let (mut server, mut client) = tokio::io::duplex(1024);
            client.write_all(&partial).await.unwrap();
            let result = next_response(&mut server).await;
            assert!(result
                .err()
                .unwrap()
                .to_string()
                .contains("control stream frame timed out"));
        }
        let (mut server, mut client) = tokio::io::duplex(1024);
        let (response, _) = tokio::join!(next_response(&mut server), async {
            tokio::time::sleep(IO_TIMEOUT * 2).await;
            write(&mut client, &Response::Logs(vec![])).await.unwrap();
        });
        assert!(matches!(response.unwrap(), Some(Response::Logs(_))));
        drop(client);
        assert!(next_response(&mut server).await.unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn test_cli_protocol_stalled_request_times_out() {
        let (mut server, _client) = tokio::io::duplex(1024);
        let read = tokio::spawn(async move { read_request(&mut server).await });
        tokio::task::yield_now().await;
        tokio::time::advance(IO_TIMEOUT + Duration::from_millis(1)).await;
        assert!(read
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("timed out"));
    }

    #[tokio::test]
    async fn test_cli_protocol_rejects_truncated_frame() {
        let (mut server, mut client) = tokio::io::duplex(1024);
        client.write_u32(100).await.unwrap();
        client.write_all(b"{}").await.unwrap();
        drop(client);
        assert!(read_request(&mut server).await.is_err());
    }
}
