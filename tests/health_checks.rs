use std::{
    future::Future,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU16, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    extract::State,
    http::{Method, StatusCode, Uri},
    routing::get,
    Router,
};
use devd::{
    config::HealthCheck,
    core::health_check::{
        HealthCheckError, HealthChecker, HealthMonitor, HealthState, ProbeFailure, ProbeResult,
    },
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpListener,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("health-check test timed out")
}

struct ServerState {
    status: AtomicU16,
    delay_ms: AtomicU64,
    requests: mpsc::UnboundedSender<(Method, Uri)>,
}

struct TestServer {
    address: SocketAddr,
    state: Arc<ServerState>,
    requests: mpsc::UnboundedReceiver<(Method, Uri)>,
    task: JoinHandle<()>,
}

impl TestServer {
    async fn new(status: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (requests, receiver) = mpsc::unbounded_channel();
        let state = Arc::new(ServerState {
            status: AtomicU16::new(status),
            delay_ms: AtomicU64::new(0),
            requests,
        });
        let app = Router::new()
            .route("/health", get(health))
            .route("/ready", get(health))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            address,
            state,
            requests: receiver,
            task,
        }
    }

    fn config(&self) -> HealthCheck {
        HealthCheck::Http {
            url: format!("http://{}/health", self.address),
            interval: Duration::from_millis(40),
            timeout: Duration::from_millis(200),
            retries: 2,
        }
    }

    async fn request(&mut self) -> (Method, Uri) {
        bounded(self.requests.recv())
            .await
            .expect("server request channel closed")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn health(
    State(state): State<Arc<ServerState>>,
    method: Method,
    uri: Uri,
) -> impl axum::response::IntoResponse {
    let status = if uri.path() == "/ready" {
        StatusCode::OK
    } else {
        StatusCode::from_u16(state.status.load(Ordering::SeqCst)).unwrap()
    };
    let delay = state.delay_ms.load(Ordering::SeqCst);
    let _ = state.requests.send((method, uri));
    if delay != 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    (status, [("location", "/ready")], "health")
}

fn tcp_config(address: SocketAddr) -> HealthCheck {
    HealthCheck::Tcp {
        host: address.ip().to_string(),
        port: address.port(),
        interval: Duration::from_millis(40),
        timeout: Duration::from_millis(200),
        retries: 2,
    }
}

#[tokio::test]
async fn test_health_tcp_reports_success_and_connection_refusal() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = tcp_config(listener.local_addr().unwrap());
    let checker = HealthChecker::new(&config).unwrap();
    assert!(bounded(checker.probe()).await.is_healthy());
    let (stream, _) = bounded(listener.accept()).await.unwrap();
    drop(stream);
    drop(listener);
    assert!(matches!(bounded(checker.probe()).await,
        ProbeResult::Unhealthy(ProbeFailure::Tcp { source }) if source.kind() == std::io::ErrorKind::ConnectionRefused));
}

#[tokio::test]
async fn test_health_tcp_supports_ipv6_loopback() {
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let checker = HealthChecker::new(&tcp_config(listener.local_addr().unwrap())).unwrap();
    assert!(bounded(checker.probe()).await.is_healthy());
}

#[tokio::test]
async fn test_health_tcp_resolves_hostname_without_blocking_runtime() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = tcp_config(listener.local_addr().unwrap());
    let HealthCheck::Tcp { host, .. } = &mut config else {
        unreachable!()
    };
    *host = "localhost".into();
    let checker = HealthChecker::new(&config).unwrap();
    assert!(bounded(checker.probe()).await.is_healthy());
}

#[tokio::test]
async fn test_health_http_accepts_only_2xx_responses() {
    let server = TestServer::new(200).await;
    let checker = HealthChecker::new(&server.config()).unwrap();
    for code in [
        200, 204, 299, 301, 302, 303, 307, 308, 400, 401, 404, 500, 503,
    ] {
        server.state.status.store(code, Ordering::SeqCst);
        let result = bounded(checker.probe()).await;
        if (200..300).contains(&code) {
            assert!(result.is_healthy(), "status {code}: {result:?}");
        } else {
            assert!(
                matches!(result, ProbeResult::Unhealthy(ProbeFailure::HttpStatus { status }) if status.as_u16() == code),
                "status {code}"
            );
        }
    }
}

#[tokio::test]
async fn test_health_http_uses_get_and_preserves_path_and_query() {
    let mut server = TestServer::new(200).await;
    let mut config = server.config();
    let HealthCheck::Http { url, .. } = &mut config else {
        unreachable!()
    };
    url.push_str("?detail=ready");
    let checker = HealthChecker::new(&config).unwrap();
    assert!(bounded(checker.probe()).await.is_healthy());
    let (method, uri) = server.request().await;
    assert_eq!(method, Method::GET);
    assert_eq!(
        uri.path_and_query().unwrap().as_str(),
        "/health?detail=ready"
    );
}

#[tokio::test]
async fn test_health_http_does_not_follow_redirect_to_healthy_endpoint() {
    let mut server = TestServer::new(302).await;
    let checker = HealthChecker::new(&server.config()).unwrap();
    assert!(matches!(
        bounded(checker.probe()).await,
        ProbeResult::Unhealthy(ProbeFailure::HttpStatus {
            status: StatusCode::FOUND
        })
    ));
    assert_eq!(server.request().await.1.path(), "/health");
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn test_health_http_connection_errors_are_unhealthy_results() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = HealthCheck::Http {
        url: format!("http://{}/health", listener.local_addr().unwrap()),
        interval: Duration::from_millis(40),
        timeout: Duration::from_millis(200),
        retries: 2,
    };
    drop(listener);
    let checker = HealthChecker::new(&config).unwrap();
    assert!(
        matches!(bounded(checker.probe()).await, ProbeResult::Unhealthy(ProbeFailure::Http { source }) if source.is_connect())
    );
}

#[tokio::test]
async fn test_health_http_timeout_does_not_block_other_runtime_tasks() {
    let mut server = TestServer::new(200).await;
    server.state.delay_ms.store(1000, Ordering::SeqCst);
    let mut config = server.config();
    let HealthCheck::Http { timeout, .. } = &mut config else {
        unreachable!()
    };
    *timeout = Duration::from_millis(50);
    let checker = HealthChecker::new(&config).unwrap();
    let (result, request) =
        bounded(async { tokio::join!(checker.probe(), server.request()) }).await;
    assert_eq!(request.0, Method::GET);
    assert!(
        matches!(result, ProbeResult::Unhealthy(ProbeFailure::Timeout { timeout }) if timeout == Duration::from_millis(50))
    );
}

#[tokio::test]
async fn test_health_http_probe_does_not_wait_for_response_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let config = HealthCheck::Http {
        url: format!("http://{address}/health"),
        interval: Duration::from_millis(40),
        timeout: Duration::from_millis(200),
        retries: 2,
    };
    let checker = HealthChecker::new(&config).unwrap();
    let (release, held) = oneshot::channel::<()>();
    let serve = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Valid headers with a body deliberately withheld until the probe ends.
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\n")
            .await
            .unwrap();
        let _ = held.await;
    };
    let probe = async {
        let result = checker.probe().await;
        let _ = release.send(());
        result
    };
    let ((), result) = bounded(async { tokio::join!(serve, probe) }).await;
    assert!(result.is_healthy());
}

#[tokio::test]
async fn test_health_monitor_retries_failures_and_resets_after_success() {
    let server = TestServer::new(503).await;
    let checker = HealthChecker::new(&server.config()).unwrap();
    let mut monitor = HealthMonitor::new(checker);
    let first = bounded(monitor.next_check()).await;
    assert_eq!(
        (first.consecutive_failures, first.state),
        (1, HealthState::Retrying)
    );
    let second = bounded(monitor.next_check()).await;
    assert_eq!(
        (second.consecutive_failures, second.state),
        (2, HealthState::Unhealthy)
    );
    server.state.status.store(204, Ordering::SeqCst);
    let recovered = bounded(monitor.next_check()).await;
    assert_eq!(
        (recovered.consecutive_failures, recovered.state),
        (0, HealthState::Healthy)
    );
    server.state.status.store(503, Ordering::SeqCst);
    let failed_again = bounded(monitor.next_check()).await;
    assert_eq!(
        (failed_again.consecutive_failures, failed_again.state),
        (1, HealthState::Retrying)
    );
}

#[tokio::test]
async fn test_health_monitor_observes_poll_interval_and_can_cancel_tick_wait() {
    let server = TestServer::new(200).await;
    let mut config = server.config();
    let HealthCheck::Http { interval, .. } = &mut config else {
        unreachable!()
    };
    *interval = Duration::from_millis(200);
    let mut monitor = HealthMonitor::new(HealthChecker::new(&config).unwrap());
    let start = tokio::time::Instant::now();
    assert_eq!(
        bounded(monitor.next_check()).await.state,
        HealthState::Healthy
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(10), monitor.next_check())
            .await
            .is_err()
    );
    assert_eq!(
        bounded(monitor.next_check()).await.state,
        HealthState::Healthy
    );
    assert!(start.elapsed() >= Duration::from_millis(200));
}

#[tokio::test]
async fn test_health_monitor_cancelled_probe_does_not_increment_failures() {
    let mut server = TestServer::new(503).await;
    let mut monitor = HealthMonitor::new(HealthChecker::new(&server.config()).unwrap());
    assert_eq!(bounded(monitor.next_check()).await.consecutive_failures, 1);
    server.request().await;
    server.state.delay_ms.store(1000, Ordering::SeqCst);
    {
        let next = monitor.next_check();
        tokio::pin!(next);
        tokio::select! {
            _ = server.request() => {},
            result = &mut next => panic!("delayed probe completed too soon: {result:?}"),
        }
    }
    server.state.delay_ms.store(0, Ordering::SeqCst);
    let next = bounded(monitor.next_check()).await;
    assert_eq!(
        (next.consecutive_failures, next.state),
        (2, HealthState::Unhealthy)
    );
}

#[tokio::test]
async fn test_health_monitor_counts_timeouts_as_consecutive_failures() {
    let server = TestServer::new(200).await;
    server.state.delay_ms.store(1000, Ordering::SeqCst);
    let mut config = server.config();
    let HealthCheck::Http { timeout, .. } = &mut config else {
        unreachable!()
    };
    *timeout = Duration::from_millis(30);
    let mut monitor = HealthMonitor::new(HealthChecker::new(&config).unwrap());
    let first = bounded(monitor.next_check()).await;
    assert!(matches!(
        first.result,
        ProbeResult::Unhealthy(ProbeFailure::Timeout { .. })
    ));
    assert_eq!(first.state, HealthState::Retrying);
    let second = bounded(monitor.next_check()).await;
    assert_eq!(
        (second.consecutive_failures, second.state),
        (2, HealthState::Unhealthy)
    );
}

#[tokio::test]
async fn test_health_readiness_polls_until_service_recovers() {
    let mut server = TestServer::new(503).await;
    let checker = HealthChecker::new(&server.config()).unwrap();
    let recover = async {
        server.request().await;
        server.state.status.store(200, Ordering::SeqCst);
    };
    let (result, ()) =
        bounded(async { tokio::join!(checker.wait_ready(Duration::from_secs(1)), recover) }).await;
    result.unwrap();
    assert_eq!(server.request().await.0, Method::GET);
}

#[tokio::test]
async fn test_health_readiness_deadline_preserves_last_failure() {
    let server = TestServer::new(503).await;
    let checker = HealthChecker::new(&server.config()).unwrap();
    let result = bounded(checker.wait_ready(Duration::from_millis(100))).await;
    assert!(matches!(
        result,
        Err(HealthCheckError::ReadinessTimeout {
            last_failure: Some(ProbeFailure::HttpStatus {
                status: StatusCode::SERVICE_UNAVAILABLE
            }),
            ..
        })
    ));
}

#[tokio::test]
async fn test_health_readiness_deadline_interrupts_long_probe_and_allows_retry() {
    let mut server = TestServer::new(200).await;
    server.state.delay_ms.store(1000, Ordering::SeqCst);
    let checker = HealthChecker::new(&server.config()).unwrap();
    let (result, _) = bounded(async {
        tokio::join!(
            checker.wait_ready(Duration::from_millis(50)),
            server.request()
        )
    })
    .await;
    assert!(matches!(
        result,
        Err(HealthCheckError::ReadinessTimeout {
            last_failure: None,
            ..
        })
    ));
    server.state.delay_ms.store(0, Ordering::SeqCst);
    bounded(checker.wait_ready(Duration::from_secs(1)))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_health_tcp_readiness_succeeds_on_first_probe() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let checker = HealthChecker::new(&tcp_config(listener.local_addr().unwrap())).unwrap();
    bounded(checker.wait_ready(Duration::from_secs(1)))
        .await
        .unwrap();
}
