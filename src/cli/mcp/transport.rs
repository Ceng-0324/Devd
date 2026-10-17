//! Bounded stdio framing and admission before the SDK can spawn request work.
use rmcp::{
    model::{ClientJsonRpcMessage, ErrorData, JsonRpcMessage, RequestId, ServerJsonRpcMessage},
    transport::Transport,
    RoleServer,
};
use std::{
    collections::HashMap,
    future::Future,
    io::{self, BufRead, Read, Write},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

pub(super) const MAX_INPUT: usize = 16 * 1024;
pub(super) const MAX_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_PENDING: usize = 8;

#[derive(Default)]
struct AdmissionState {
    next: i64,
    pending: HashMap<RequestId, Pending>,
}
struct Pending {
    wire_id: RequestId,
    finished: bool,
    cancelled: bool,
    sending: bool,
}
#[derive(Clone, Default)]
pub(super) struct Admissions(Arc<Mutex<AdmissionState>>);
impl Admissions {
    // Unique internal IDs prevent a delayed SDK response from retiring or
    // answering a newer request that reused a cancelled client's wire ID.
    fn admit(&self, wire_id: RequestId, tool: bool) -> Option<RequestId> {
        let mut state = self.0.lock().unwrap();
        if state.pending.len() >= MAX_PENDING
            || state.pending.values().any(|p| p.wire_id == wire_id)
        {
            return None;
        }
        state.next = state.next.checked_add(1)?;
        let id = RequestId::Number(state.next);
        state.pending.insert(
            id.clone(),
            Pending {
                wire_id,
                finished: !tool,
                cancelled: false,
                sending: false,
            },
        );
        Some(id)
    }
    fn finish(&self, id: &RequestId) {
        let mut state = self.0.lock().unwrap();
        if let Some(p) = state.pending.get_mut(id) {
            p.finished = true;
            if p.cancelled && !p.sending {
                state.pending.remove(id);
            }
        }
    }
    fn cancel(&self, wire_id: &RequestId) -> Option<RequestId> {
        let mut state = self.0.lock().unwrap();
        let id = state
            .pending
            .iter()
            .find(|(_, p)| &p.wire_id == wire_id)
            .map(|(id, _)| id.clone())?;
        let p = state.pending.get_mut(&id).unwrap();
        if p.cancelled {
            return None;
        }
        p.cancelled = true;
        if p.finished && !p.sending {
            state.pending.remove(&id);
        }
        Some(id)
    }
    fn sending(&self, id: &RequestId) -> Option<RequestId> {
        let mut state = self.0.lock().unwrap();
        let p = state.pending.get_mut(id)?;
        p.sending = true;
        Some(p.wire_id.clone())
    }
    fn retire(&self, id: &RequestId) {
        self.0.lock().unwrap().pending.remove(id);
    }
    pub(super) fn completion_guard(&self, id: RequestId) -> CompletionGuard {
        CompletionGuard {
            admissions: self.clone(),
            id,
        }
    }
}
pub(super) struct CompletionGuard {
    admissions: Admissions,
    id: RequestId,
}
impl Drop for CompletionGuard {
    fn drop(&mut self) {
        self.admissions.finish(&self.id);
    }
}

pub(super) struct Stdio<W = super::super::stdout::Stdout> {
    input: mpsc::Receiver<io::Result<Vec<u8>>>,
    output: Arc<tokio::sync::Mutex<W>>,
    pub(super) admissions: Admissions,
    shutdown: CancellationToken,
    initialized: bool,
}
impl Stdio {
    pub(super) fn new(shutdown: CancellationToken) -> io::Result<Self> {
        let (tx, input) = mpsc::channel(1);
        std::thread::Builder::new()
            .name("devd-mcp-input".into())
            .spawn(move || read(std::io::stdin().lock(), tx))?;
        Ok(Self {
            input,
            output: Arc::new(tokio::sync::Mutex::new(super::super::stdout::Stdout::new()?)),
            admissions: Admissions::default(),
            shutdown,
            initialized: false,
        })
    }
}
fn read(mut reader: impl BufRead, sender: mpsc::Sender<io::Result<Vec<u8>>>) {
    loop {
        let mut line = vec![];
        match reader
            .by_ref()
            .take((MAX_INPUT + 1) as u64)
            .read_until(b'\n', &mut line)
        {
            Ok(0) => break,
            Ok(_) if line.len() > MAX_INPUT => {
                let _ = sender.blocking_send(Err(io::Error::other("MCP input exceeds 16 KiB")));
                break;
            }
            Ok(_) => {
                if sender.blocking_send(Ok(line)).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = sender.blocking_send(Err(error));
                break;
            }
        }
    }
}

impl<W: AsyncWrite + Unpin + Send + 'static> Transport<RoleServer> for Stdio<W> {
    type Error = io::Error;
    fn send(
        &mut self,
        mut message: ServerJsonRpcMessage,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let output = self.output.clone();
        let admissions = self.admissions.clone();
        let shutdown = self.shutdown.clone();
        async move {
            let id = match &message {
                JsonRpcMessage::Response(r) => Some(r.id.clone()),
                JsonRpcMessage::Error(r) => r.id.clone(),
                _ => None,
            };
            if let Some(id) = &id {
                let Some(wire_id) = admissions.sending(id) else {
                    return Ok(());
                };
                match &mut message {
                    JsonRpcMessage::Response(r) => r.id = wire_id,
                    JsonRpcMessage::Error(r) => r.id = Some(wire_id),
                    _ => unreachable!(),
                }
            }
            let bytes = encode(&message)?;
            let result = send_bytes(&output, &bytes, &shutdown).await;
            if let Some(id) = id {
                admissions.retire(&id);
            }
            if result.is_err() {
                shutdown.cancel();
            }
            result
        }
    }
    async fn receive(&mut self) -> Option<ClientJsonRpcMessage> {
        loop {
            let line = tokio::select! { _ = self.shutdown.cancelled() => return None, line = self.input.recv() => line };
            let Some(line) = line else {
                self.shutdown.cancel();
                return None;
            };
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    eprintln!("{error}");
                    self.shutdown.cancel();
                    return None;
                }
            };
            // A typed SDK parse rejects malformed envelopes, unsupported batches
            // and invalid protocol parameter types before dispatch.
            let mut message = match serde_json::from_slice::<ClientJsonRpcMessage>(&line) {
                Ok(message) => message,
                Err(_) => {
                    let error = if serde_json::from_slice::<serde_json::Value>(&line).is_err() {
                        ErrorData::new(rmcp::model::ErrorCode::PARSE_ERROR, "invalid JSON", None)
                    } else {
                        ErrorData::invalid_request("invalid MCP request", None)
                    };
                    if self
                        .send(ServerJsonRpcMessage::error(error, None))
                        .await
                        .is_err()
                    {
                        return None;
                    }
                    continue;
                }
            };
            if let JsonRpcMessage::Request(request) = &mut message {
                let tool = matches!(
                    request.request,
                    rmcp::model::ClientRequest::CallToolRequest(_)
                );
                let Some(id) = self.admissions.admit(request.id.clone(), tool) else {
                    eprintln!("MCP session closed: duplicate request ID or more than eight outstanding requests");
                    self.shutdown.cancel();
                    return None;
                };
                request.id = id;
            }
            if let JsonRpcMessage::Notification(notification) = &mut message {
                match &mut notification.notification {
                    rmcp::model::ClientNotification::InitializedNotification(_)
                        if !self.initialized =>
                    {
                        self.initialized = true;
                    }
                    rmcp::model::ClientNotification::CancelledNotification(cancel) => {
                        let Some(id) = cancel
                            .params
                            .request_id
                            .as_ref()
                            .and_then(|id| self.admissions.cancel(id))
                        else {
                            continue;
                        };
                        cancel.params.request_id = Some(id);
                    }
                    // This adapter never requests roots, sampling or progress.
                    // Drop irrelevant/repeated notifications before SDK task spawn.
                    _ => continue,
                }
            }
            return Some(message);
        }
    }
    async fn close(&mut self) -> io::Result<()> {
        self.input.close();
        self.shutdown.cancel();
        Ok(())
    }
}

struct Bounded(Vec<u8>);
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_OUTPUT.saturating_sub(self.0.len() + 1) {
            return Err(io::Error::other("MCP output exceeds 8 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn encode(message: &ServerJsonRpcMessage) -> io::Result<Vec<u8>> {
    let mut bytes = Bounded(vec![]);
    if serde_json::to_writer(&mut bytes, message).is_err() {
        let id = match message {
            JsonRpcMessage::Response(r) => Some(r.id.clone()),
            JsonRpcMessage::Error(r) => r.id.clone(),
            _ => None,
        };
        bytes.0.clear();
        serde_json::to_writer(
            &mut bytes,
            &ServerJsonRpcMessage::error(
                ErrorData::internal_error(
                    "response exceeds 8 MiB; reduce event tail or omit logs",
                    None,
                ),
                id,
            ),
        )?;
    }
    bytes.0.push(b'\n');
    Ok(bytes.0)
}
async fn send_bytes(
    output: &tokio::sync::Mutex<impl AsyncWrite + Unpin>,
    bytes: &[u8],
    shutdown: &CancellationToken,
) -> io::Result<()> {
    tokio::select! {
        _ = shutdown.cancelled() => Err(io::Error::other("MCP session closed")),
        result = tokio::time::timeout(Duration::from_secs(5), async {
            let mut out = output.lock().await;
            out.write_all(bytes).await?;
            out.flush().await
        }) => result.unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "MCP output stalled"))),
    }
}

#[cfg(test)]
#[path = "transport/lifecycle_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn test_mcp_output_backpressure_and_lock_wait_are_bounded_and_cancellable() {
        let (out, _unread) = tokio::io::duplex(1);
        let out = tokio::sync::Mutex::new(out);
        let shutdown = CancellationToken::new();
        assert_eq!(
            send_bytes(&out, b"blocked", &shutdown)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        let _lock = out.lock().await;
        assert_eq!(
            send_bytes(&out, b"blocked", &shutdown)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        shutdown.cancel();
        let start = tokio::time::Instant::now();
        assert!(send_bytes(&out, b"cancelled", &shutdown).await.is_err());
        assert_eq!(start.elapsed(), Duration::ZERO);
    }
    #[test]
    fn test_mcp_input_bounds_before_allocating_a_full_line() {
        let (tx, mut rx) = mpsc::channel(1);
        read(io::Cursor::new(vec![b'x'; MAX_INPUT + 10]), tx);
        assert!(rx.try_recv().unwrap().is_err());
    }
    #[test]
    fn test_mcp_admission_bounds_duplicates_and_cancel_retirement() {
        let a = Admissions::default();
        let ids: Vec<_> = (0..8)
            .map(|n| a.admit(RequestId::Number(n), true).unwrap())
            .collect();
        assert!(a.admit(RequestId::Number(8), true).is_none());
        assert!(a.admit(RequestId::Number(1), true).is_none());
        let guard = a.completion_guard(ids[1].clone());
        assert_eq!(a.cancel(&RequestId::Number(1)), Some(ids[1].clone()));
        assert!(a.admit(RequestId::Number(8), true).is_none());
        drop(guard);
        let reused = a.admit(RequestId::Number(1), true).unwrap();
        assert_ne!(reused, ids[1]);
        assert!(a.sending(&ids[1]).is_none());
        a.retire(&ids[1]);
        assert_eq!(a.sending(&reused), Some(RequestId::Number(1)));
        a.cancel(&RequestId::Number(1));
        a.finish(&reused);
        assert!(a.admit(RequestId::Number(8), true).is_none());
        a.retire(&reused);
        assert!(a.admit(RequestId::Number(8), true).is_some());
    }
    #[test]
    fn test_mcp_output_limit_returns_correlated_error() {
        let result = rmcp::model::ServerResult::CallToolResult(super::super::tools::result(
            serde_json::json!({"large":"x".repeat(MAX_OUTPUT)}),
        ));
        let message = ServerJsonRpcMessage::response(result, RequestId::Number(4));
        let bytes = encode(&message).unwrap();
        let reply: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reply["id"], 4);
        assert_eq!(reply["error"]["code"], -32603);
        assert!(bytes.len() < 1024);
    }
}
