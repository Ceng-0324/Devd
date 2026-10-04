use axum::{extract::State, http::StatusCode, routing::get, Router};
use std::{
    net::TcpListener,
    sync::{
        atomic::{AtomicU16, AtomicUsize, Ordering},
        Arc,
    },
    thread,
};
use tokio::sync::oneshot;

#[derive(Clone)]
pub struct Probe {
    status: Arc<AtomicU16>,
    requests: Arc<AtomicUsize>,
}

impl Probe {
    fn new() -> Self {
        Self {
            status: Arc::new(AtomicU16::new(503)),
            requests: Arc::new(AtomicUsize::new(0)),
        }
    }
    pub fn set(&self, status: u16) {
        self.status.store(status, Ordering::SeqCst);
    }
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

async fn health(State(probe): State<Probe>) -> StatusCode {
    probe.requests.fetch_add(1, Ordering::SeqCst);
    StatusCode::from_u16(probe.status.load(Ordering::SeqCst)).unwrap()
}

/// Test-owned readiness endpoints. The socket is bound to port zero and kept
/// open throughout, avoiding reserve/drop/rebind races and external services.
pub struct HttpMock {
    pub url: String,
    pub database: Probe,
    pub api: Probe,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl HttpMock {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let database = Probe::new();
        let api = Probe::new();
        let app = Router::new()
            .route("/database", get(health).with_state(database.clone()))
            .route("/api", get(health).with_state(api.clone()));
        let (stop, stopped) = oneshot::channel();
        let thread = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async {
                            let _ = stopped.await;
                        })
                        .await
                        .unwrap();
                });
        });
        Self {
            url,
            database,
            api,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for HttpMock {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
