use std::{path::Path, time::Duration};

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

use crate::{
    core::service_manager::{RuntimeSnapshot, ServiceSnapshot},
    logging::LogEntry,
};

const MAX_REQUEST: usize = 4096;
const MAX_RESPONSE: usize = 128 * 1024 * 1024;
pub(super) const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum Request {
    Status,
    Stop,
    Restart {
        service: String,
    },
    Logs {
        service: Option<String>,
        tail: usize,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "result", content = "data", rename_all = "kebab-case")]
pub(super) enum Response {
    Status(RuntimeSnapshot),
    Stopping,
    Restarted(ServiceSnapshot),
    Logs(Vec<LogEntry>),
    Error(String),
}

async fn read<T: DeserializeOwned>(stream: &mut UnixStream, maximum: usize) -> Result<T> {
    let length = stream.read_u32().await? as usize;
    if length > maximum {
        bail!("control message exceeds {maximum} bytes");
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).context("invalid control message")
}

pub(super) async fn read_request(stream: &mut UnixStream) -> Result<Request> {
    tokio::time::timeout(IO_TIMEOUT, read(stream, MAX_REQUEST))
        .await
        .context("control request timed out")?
}

pub(super) async fn write<T: Serialize>(stream: &mut UnixStream, message: &T) -> Result<()> {
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
    let mut stream = tokio::time::timeout(IO_TIMEOUT, UnixStream::connect(socket)).await?
        .with_context(|| format!("no reachable devd supervisor at {}; run 'devd start' with the same --config and --state-dir", socket.display()))?;
    write(&mut stream, &message).await?;
    let response = tokio::time::timeout(Duration::from_secs(60), read(&mut stream, MAX_RESPONSE))
        .await
        .context("supervisor response timed out; inspect 'devd status' before retrying")??;
    match response {
        Response::Error(error) => bail!("{error}"),
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn test_cli_protocol_stalled_request_times_out() {
        let (mut server, _client) = UnixStream::pair().unwrap();
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
        let (mut server, mut client) = UnixStream::pair().unwrap();
        client.write_u32(100).await.unwrap();
        client.write_all(b"{}").await.unwrap();
        drop(client);
        assert!(read_request(&mut server).await.is_err());
    }
}
