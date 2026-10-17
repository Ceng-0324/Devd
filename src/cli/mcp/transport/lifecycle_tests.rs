//! Exercise the real adapter, SDK loop and admission transport together.
use super::*;
use crate::cli::{
    control_session::{AdmissionState as ControlAdmission, Policy, Session},
    instances::Identity,
    mcp::{Adapter, Source},
    protocol::{self, Request},
};
use serde_json::{json, Value};
use tokio::{io::AsyncBufReadExt, sync::Semaphore};

#[tokio::test]
async fn test_mcp_nine_adapter_timeouts_send_responses_and_release_admissions() {
    #[cfg(unix)]
    let root = tempfile::tempdir_in("/tmp").unwrap();
    #[cfg(windows)]
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("control.sock");
    let mut listener = crate::cli::transport::Listener::bind(&socket)
        .await
        .unwrap();
    let (observed, mut requests) = mpsc::channel(1);
    let endpoint = tokio::spawn(async move {
        while let Ok(mut stream) = listener.accept().await {
            assert!(matches!(protocol::read_request(&mut stream).await.unwrap(),
                Request::Scoped { request, .. } if matches!(*request, Request::Status)));
            // The test owns the accepted connection and deliberately withholds
            // its response until the adapter's 30s deadline has fired.
            if observed.send(stream).await.is_err() {
                break;
            }
        }
    });
    let session = Session {
        identity: Identity {
            schema_version: 1,
            instance_id: "instance".into(),
            run_id: "run".into(),
            supervisor_pid: 1,
            started_at: chrono::Utc::now(),
            config: "devd.yml".into(),
            state_dir: root.path().into(),
            profile: None,
            project_root: root.path().into(),
            git: None,
        },
        socket,
        allow: vec![],
        policy: Policy::default(),
        admission: Arc::new(Mutex::new(ControlAdmission::default())),
        controls: Arc::new(Semaphore::new(1)),
    };
    let (input, incoming) = mpsc::channel(1);
    let (writer, reader) = tokio::io::duplex(16 * 1024);
    let admissions = Admissions::default();
    let shutdown = CancellationToken::new();
    let transport = Stdio {
        input: incoming,
        output: Arc::new(tokio::sync::Mutex::new(writer)),
        admissions: admissions.clone(),
        shutdown: shutdown.clone(),
        initialized: true,
    };
    let adapter = Adapter {
        source: Source::Live(Box::new(session)),
        admissions: admissions.clone(),
        shutdown: shutdown.clone(),
    };
    // Skip only the handshake; every request/response uses the real SDK loop.
    let service = rmcp::service::serve_directly(adapter, transport, None);
    let mut replies = tokio::io::BufReader::new(reader).lines();
    for _ in 0..9 {
        input
            .send(Ok(serde_json::to_vec(&json!({
                "jsonrpc":"2.0", "id":"reused-wire-id", "method":"tools/call",
                "params":{"name":"devd_status","arguments":{}}
            }))
            .unwrap()))
            .await
            .unwrap();
        let stream = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        {
            let state = admissions.0.lock().unwrap();
            assert_eq!(state.pending.len(), 1);
            assert!(!state.pending.values().next().unwrap().finished);
        }
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(30)).await;
        let response: Value = serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(1), replies.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        tokio::time::resume();
        assert_eq!(response["id"], "reused-wire-id");
        assert_eq!(response["result"]["isError"], true);
        let failure: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(failure["code"], "timed-out", "{response}");
        assert!(admissions.0.lock().unwrap().pending.is_empty());
        assert!(!shutdown.is_cancelled());
        drop(stream);
    }
    drop(input);
    service.cancel().await.unwrap();
    endpoint.abort();
    let _ = endpoint.await;
}
