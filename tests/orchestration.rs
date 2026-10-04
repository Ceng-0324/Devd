#![cfg(unix)]

use std::{
    collections::HashMap,
    future::Future,
    path::Path,
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{extract::State, http::StatusCode, routing::get, Router};
use devd::{
    config::{
        BackoffType, Dependency, DependencyCondition, DevdConfig, HealthCheck, RestartPolicyType,
        ServiceConfig,
    },
    core::service_manager::{
        ManagerOptions, OutputStream, ProcessOutput, RuntimeSnapshot, ServiceManager,
        ServiceManagerError, ServiceState,
    },
};
use nix::{
    errno::Errno,
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use tempfile::{tempdir, TempDir};
use tokio::{
    net::TcpListener,
    sync::{broadcast, oneshot, watch},
    task::JoinHandle,
};

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("orchestration test timed out")
}

fn service(script: &str, directory: &Path) -> ServiceConfig {
    let mut config: ServiceConfig =
        serde_yaml::from_str("command: test\nrestart: {policy: never}").unwrap();
    config.command = shell_words::join(["/bin/sh", "-c", script]);
    config
        .env
        .insert("DEVD_TEST_DIR".into(), directory.to_string_lossy().into());
    config.restart.initial_delay = Duration::from_millis(20);
    config
}

fn sleeper(directory: &Path) -> ServiceConfig {
    service("trap 'exit 0' TERM; sleep 60 & wait", directory)
}

fn config(services: impl IntoIterator<Item = (&'static str, ServiceConfig)>) -> DevdConfig {
    DevdConfig {
        version: "1".into(),
        services: services
            .into_iter()
            .map(|(name, service)| (name.into(), service))
            .collect(),
    }
}

fn dependency(service: &str, condition: DependencyCondition) -> Dependency {
    Dependency {
        service: service.into(),
        condition,
    }
}

fn options(directory: &TempDir) -> ManagerOptions {
    let mut options = ManagerOptions::new(directory.path().join("state/services.json"));
    options.grace_period = Duration::from_millis(100);
    options.dependency_timeout = Duration::from_secs(2);
    options
}

struct RunningManager {
    task: Option<JoinHandle<Result<RuntimeSnapshot, ServiceManagerError>>>,
    stop: Option<oneshot::Sender<()>>,
    snapshots: watch::Receiver<RuntimeSnapshot>,
    output: broadcast::Receiver<ProcessOutput>,
}

impl RunningManager {
    fn start(manager: ServiceManager) -> Self {
        let snapshots = manager.subscribe();
        let output = manager.subscribe_output();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(manager.run_until(async {
            let _ = stopped.await;
        }));
        Self {
            task: Some(task),
            stop: Some(stop),
            snapshots,
            output,
        }
    }

    async fn until(&mut self, predicate: impl Fn(&RuntimeSnapshot) -> bool) -> RuntimeSnapshot {
        bounded(async {
            loop {
                let snapshot = self.snapshots.borrow_and_update().clone();
                if predicate(&snapshot) {
                    return snapshot;
                }
                self.snapshots
                    .changed()
                    .await
                    .expect("manager exited before expected state");
            }
        })
        .await
    }

    async fn finish(&mut self) -> Result<RuntimeSnapshot, ServiceManagerError> {
        bounded(self.task.take().unwrap()).await.unwrap()
    }

    async fn shutdown(&mut self) -> Result<RuntimeSnapshot, ServiceManagerError> {
        let _ = self.stop.take().unwrap().send(());
        self.finish().await
    }
}

impl Drop for RunningManager {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn reaped(pid: u32) {
    bounded(async {
        loop {
            match kill(Pid::from_raw(pid as i32), None) {
                Err(Errno::ESRCH) => return,
                Ok(()) => tokio::time::sleep(Duration::from_millis(10)).await,
                error => panic!("unexpected PID result: {error:?}"),
            }
        }
    })
    .await;
}

#[tokio::test]
async fn test_orchestration_independent_services_start_concurrently() {
    let directory = tempdir().unwrap();
    let a = service("touch \"$DEVD_TEST_DIR/a\"; while [ ! -f \"$DEVD_TEST_DIR/b\" ]; do sleep 0.01; done; echo both-ready; sleep 60 & wait", directory.path());
    let b = service("touch \"$DEVD_TEST_DIR/b\"; while [ ! -f \"$DEVD_TEST_DIR/a\" ]; do sleep 0.01; done; echo both-ready; sleep 60 & wait", directory.path());
    let mut run = RunningManager::start(
        ServiceManager::new(config([("a", a), ("b", b)]), options(&directory)).unwrap(),
    );
    let mut outputs = HashMap::<String, Vec<u8>>::new();
    bounded(async {
        while outputs
            .values()
            .filter(|bytes| bytes.windows(10).any(|s| s == b"both-ready"))
            .count()
            < 2
        {
            let output = run.output.recv().await.unwrap();
            outputs
                .entry(output.service)
                .or_default()
                .extend(output.bytes);
        }
    })
    .await;
    let snapshot = run
        .until(|s| {
            s.services
                .values()
                .all(|s| s.status == ServiceState::Running)
        })
        .await;
    let pids: Vec<_> = snapshot.services.values().map(|s| s.pid.unwrap()).collect();
    let stopped = run.shutdown().await;
    assert!(stopped.is_ok(), "{stopped:?}; {:?}", run.snapshots.borrow());
    assert!(stopped
        .unwrap()
        .services
        .values()
        .all(|s| s.status == ServiceState::Stopped));
    for pid in pids {
        reaped(pid).await;
    }
}

#[tokio::test]
async fn test_orchestration_started_condition_does_not_wait_for_health() {
    let directory = tempdir().unwrap();
    let mut root = sleeper(directory.path());
    root.healthcheck = Some(HealthCheck::Http {
        url: "http://127.0.0.1:1/health".into(),
        interval: Duration::from_millis(20),
        timeout: Duration::from_millis(100),
        retries: 1,
    });
    let mut dependent = sleeper(directory.path());
    dependent
        .depends_on
        .push(dependency("root", DependencyCondition::Started));
    let mut run = RunningManager::start(
        ServiceManager::new(
            config([("root", root), ("dependent", dependent)]),
            options(&directory),
        )
        .unwrap(),
    );
    let snapshot = run
        .until(|s| {
            s.services["dependent"].pid.is_some()
                && s.services["root"].status == ServiceState::Unhealthy
        })
        .await;
    assert!(snapshot.services["root"].pid.is_some());
    run.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_orchestration_tcp_ready_blocks_dependent_until_connectable() {
    let directory = tempdir().unwrap();
    let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reserved.local_addr().unwrap();
    drop(reserved);
    let mut root = sleeper(directory.path());
    root.healthcheck = Some(HealthCheck::Tcp {
        host: "127.0.0.1".into(),
        port: address.port(),
        interval: Duration::from_millis(20),
        timeout: Duration::from_millis(100),
        retries: 1,
    });
    let mut dependent = service(
        "touch \"$DEVD_TEST_DIR/dependent\"; sleep 60 & wait",
        directory.path(),
    );
    dependent
        .depends_on
        .push(dependency("root", DependencyCondition::TcpReady));
    let mut run = RunningManager::start(
        ServiceManager::new(
            config([("root", root), ("dependent", dependent)]),
            options(&directory),
        )
        .unwrap(),
    );
    run.until(|s| s.services["root"].status == ServiceState::Unhealthy)
        .await;
    assert!(!directory.path().join("dependent").exists());
    let listener = TcpListener::bind(address).await.unwrap();
    run.until(|s| s.services["dependent"].pid.is_some()).await;
    run.shutdown().await.unwrap();
    drop(listener);
}

struct HttpServer {
    status: Arc<AtomicU16>,
    address: std::net::SocketAddr,
    task: JoinHandle<()>,
}

impl HttpServer {
    async fn start(status: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let status = Arc::new(AtomicU16::new(status));
        let app = Router::new()
            .route(
                "/health",
                get(|State(status): State<Arc<AtomicU16>>| async move {
                    StatusCode::from_u16(status.load(Ordering::SeqCst)).unwrap()
                }),
            )
            .with_state(status.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            status,
            address,
            task,
        }
    }
    fn healthcheck(&self) -> HealthCheck {
        HealthCheck::Http {
            url: format!("http://{}/health", self.address),
            interval: Duration::from_millis(20),
            timeout: Duration::from_millis(100),
            retries: 1,
        }
    }
}
impl Drop for HttpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn test_orchestration_http_ready_and_never_policy_recover_without_restart() {
    let directory = tempdir().unwrap();
    let server = HttpServer::start(503).await;
    let mut root = sleeper(directory.path());
    root.healthcheck = Some(server.healthcheck());
    let mut dependent = sleeper(directory.path());
    dependent
        .depends_on
        .push(dependency("root", DependencyCondition::HttpReady));
    let mut run = RunningManager::start(
        ServiceManager::new(
            config([("root", root), ("dependent", dependent)]),
            options(&directory),
        )
        .unwrap(),
    );
    let initial = run
        .until(|s| s.services["root"].status == ServiceState::Unhealthy)
        .await;
    assert_eq!(initial.services["dependent"].status, ServiceState::Pending);
    server.status.store(200, Ordering::SeqCst);
    let ready = run.until(|s| s.services["dependent"].pid.is_some()).await;
    assert_eq!(ready.services["root"].pid, initial.services["root"].pid);
    assert_eq!(ready.services["root"].restart_count, 0);
    run.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_orchestration_dependency_timeout_rolls_back_running_stack() {
    let directory = tempdir().unwrap();
    let server = HttpServer::start(503).await;
    let mut root = sleeper(directory.path());
    root.healthcheck = Some(server.healthcheck());
    let mut dependent = sleeper(directory.path());
    dependent
        .depends_on
        .push(dependency("root", DependencyCondition::HttpReady));
    let mut settings = options(&directory);
    settings.dependency_timeout = Duration::from_millis(150);
    let mut run = RunningManager::start(
        ServiceManager::new(config([("root", root), ("dependent", dependent)]), settings).unwrap(),
    );
    let initial = run.until(|s| s.services["root"].pid.is_some()).await;
    assert!(
        matches!(run.finish().await, Err(ServiceManagerError::FailedServices { failures }) if failures.keys().collect::<Vec<_>>() == ["dependent"])
    );
    let final_state = run.snapshots.borrow().clone();
    assert!(final_state.services["dependent"]
        .last_error
        .as_ref()
        .unwrap()
        .contains("timed out"));
    assert!(final_state.services.values().all(|s| s.pid.is_none()));
    reaped(initial.services["root"].pid.unwrap()).await;
}

#[tokio::test]
async fn test_orchestration_failure_restart_recovers_and_counts_attempts() {
    let directory = tempdir().unwrap();
    let mut child = service("if [ ! -f \"$DEVD_TEST_DIR/attempt\" ]; then touch \"$DEVD_TEST_DIR/attempt\"; exit 7; fi; echo recovered; sleep 60 & wait", directory.path());
    child.restart.policy = RestartPolicyType::OnFailure;
    let mut run = RunningManager::start(
        ServiceManager::new(config([("child", child)]), options(&directory)).unwrap(),
    );
    let state = run
        .until(|s| s.services["child"].restart_count == 1 && s.services["child"].pid.is_some())
        .await;
    assert_eq!(state.services["child"].last_exit_code, Some(7));
    run.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_orchestration_restart_budget_stops_failure_loop() {
    let directory = tempdir().unwrap();
    let mut child = service("exit 9", directory.path());
    child.restart.policy = RestartPolicyType::Always;
    child.restart.max_attempts = 2;
    let mut run = RunningManager::start(
        ServiceManager::new(config([("child", child)]), options(&directory)).unwrap(),
    );
    assert!(matches!(
        run.finish().await,
        Err(ServiceManagerError::FailedServices { .. })
    ));
    let state = run.snapshots.borrow().clone();
    assert_eq!(state.services["child"].restart_count, 2);
    assert_eq!(state.services["child"].last_exit_code, Some(9));
    assert_eq!(state.services["child"].status, ServiceState::Failed);
}

#[tokio::test]
async fn test_orchestration_clean_exit_on_failure_stops_without_restart() {
    let directory = tempdir().unwrap();
    let mut child = service("exit 0", directory.path());
    child.restart.policy = RestartPolicyType::OnFailure;
    let mut run = RunningManager::start(
        ServiceManager::new(config([("child", child)]), options(&directory)).unwrap(),
    );
    let state = run.finish().await.unwrap();
    assert_eq!(state.services["child"].status, ServiceState::Stopped);
    assert_eq!(state.services["child"].restart_count, 0);
}

#[tokio::test]
async fn test_orchestration_always_restarts_clean_exit() {
    let directory = tempdir().unwrap();
    let mut child = service("if [ ! -f \"$DEVD_TEST_DIR/attempt\" ]; then touch \"$DEVD_TEST_DIR/attempt\"; exit 0; fi; sleep 60 & wait", directory.path());
    child.restart.policy = RestartPolicyType::Always;
    let mut run = RunningManager::start(
        ServiceManager::new(config([("child", child)]), options(&directory)).unwrap(),
    );
    run.until(|s| s.services["child"].restart_count == 1 && s.services["child"].pid.is_some())
        .await;
    run.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_orchestration_health_failure_restarts_running_process() {
    let directory = tempdir().unwrap();
    let server = HttpServer::start(200).await;
    let mut child = sleeper(directory.path());
    child.restart.policy = RestartPolicyType::OnFailure;
    child.healthcheck = Some(server.healthcheck());
    child.restart.initial_delay = Duration::from_millis(100);
    let mut run = RunningManager::start(
        ServiceManager::new(config([("child", child)]), options(&directory)).unwrap(),
    );
    let initial = run
        .until(|s| s.services["child"].status == ServiceState::Healthy)
        .await;
    server.status.store(503, Ordering::SeqCst);
    run.until(|s| s.services["child"].status == ServiceState::Restarting)
        .await;
    server.status.store(200, Ordering::SeqCst);
    let ready = run
        .until(|s| {
            s.services["child"].status == ServiceState::Healthy
                && s.services["child"].restart_count == 1
        })
        .await;
    assert_ne!(initial.services["child"].pid, ready.services["child"].pid);
    reaped(initial.services["child"].pid.unwrap()).await;
    run.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_orchestration_spawn_failure_stops_started_siblings() {
    let directory = tempdir().unwrap();
    let mut broken = sleeper(directory.path());
    broken.command = "/devd-test-missing-executable".into();
    let mut dependent = service("touch \"$DEVD_TEST_DIR/dependent\"", directory.path());
    dependent
        .depends_on
        .push(dependency("broken", DependencyCondition::Started));
    let mut run = RunningManager::start(
        ServiceManager::new(
            config([
                ("broken", broken),
                ("dependent", dependent),
                ("sibling", sleeper(directory.path())),
            ]),
            options(&directory),
        )
        .unwrap(),
    );
    assert!(matches!(
        run.finish().await,
        Err(ServiceManagerError::FailedServices { .. })
    ));
    assert!(!directory.path().join("dependent").exists());
    assert!(run
        .snapshots
        .borrow()
        .services
        .values()
        .all(|s| s.pid.is_none()));
}

#[tokio::test]
async fn test_orchestration_spawn_failure_can_retry_after_environment_file_is_created() {
    let directory = tempdir().unwrap();
    let mut child = sleeper(directory.path());
    child.env_file = Some(directory.path().join(".env"));
    child.restart.policy = RestartPolicyType::OnFailure;
    child.restart.initial_delay = Duration::from_millis(200);
    let mut run = RunningManager::start(
        ServiceManager::new(config([("child", child)]), options(&directory)).unwrap(),
    );
    run.until(|s| s.services["child"].status == ServiceState::Restarting)
        .await;
    tokio::fs::write(directory.path().join(".env"), "DEVD_RECOVERED=yes\n")
        .await
        .unwrap();
    let recovered = run.until(|s| s.services["child"].pid.is_some()).await;
    assert_eq!(recovered.services["child"].restart_count, 1);
    assert!(recovered.services["child"].last_error.is_none());
    run.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_orchestration_restart_rechecks_dependency_readiness() {
    let directory = tempdir().unwrap();
    let server = HttpServer::start(200).await;
    let mut root = sleeper(directory.path());
    root.healthcheck = Some(server.healthcheck());
    let mut child = sleeper(directory.path());
    child.restart.policy = RestartPolicyType::Always;
    child.restart.initial_delay = Duration::from_millis(100);
    child
        .depends_on
        .push(dependency("root", DependencyCondition::HttpReady));
    let mut run = RunningManager::start(
        ServiceManager::new(
            config([("root", root), ("child", child)]),
            options(&directory),
        )
        .unwrap(),
    );
    let initial = run.until(|s| s.services["child"].pid.is_some()).await;
    server.status.store(503, Ordering::SeqCst);
    run.until(|s| s.services["root"].status == ServiceState::Unhealthy)
        .await;
    kill(
        Pid::from_raw(initial.services["child"].pid.unwrap() as i32),
        Signal::SIGKILL,
    )
    .unwrap();
    run.until(|s| {
        s.services["child"].status == ServiceState::Restarting && s.services["child"].pid.is_none()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(run.snapshots.borrow().services["child"].pid.is_none());
    assert_eq!(run.snapshots.borrow().services["child"].restart_count, 0);
    server.status.store(200, Ordering::SeqCst);
    run.until(|s| s.services["child"].pid.is_some() && s.services["child"].restart_count == 1)
        .await;
    run.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_orchestration_health_failure_exhaustion_stops_process() {
    let directory = tempdir().unwrap();
    let server = HttpServer::start(200).await;
    let mut child = sleeper(directory.path());
    child.healthcheck = Some(server.healthcheck());
    child.restart.policy = RestartPolicyType::OnFailure;
    child.restart.max_attempts = 0;
    let mut run = RunningManager::start(
        ServiceManager::new(config([("child", child)]), options(&directory)).unwrap(),
    );
    let initial = run
        .until(|s| s.services["child"].status == ServiceState::Healthy)
        .await;
    server.status.store(503, Ordering::SeqCst);
    assert!(matches!(
        run.finish().await,
        Err(ServiceManagerError::FailedServices { .. })
    ));
    assert_eq!(run.snapshots.borrow().services["child"].restart_count, 0);
    reaped(initial.services["child"].pid.unwrap()).await;
}

#[tokio::test]
async fn test_orchestration_shutdown_forces_stubborn_leader_and_descendants() {
    let directory = tempdir().unwrap();
    let child = service("trap '' TERM; sleep 60 & echo $!; wait", directory.path());
    let mut run = RunningManager::start(
        ServiceManager::new(config([("child", child)]), options(&directory)).unwrap(),
    );
    let output = bounded(run.output.recv()).await.unwrap();
    let descendant: u32 = String::from_utf8(output.bytes)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let initial = run.until(|s| s.services["child"].pid.is_some()).await;
    let final_state = run.shutdown().await.unwrap();
    assert_eq!(final_state.services["child"].status, ServiceState::Stopped);
    assert_eq!(
        final_state.services["child"].last_exit_signal,
        Some(Signal::SIGKILL as i32)
    );
    reaped(initial.services["child"].pid.unwrap()).await;
    reaped(descendant).await;
}

#[tokio::test]
async fn test_orchestration_state_initialization_failure_spawns_no_children() {
    let directory = tempdir().unwrap();
    let mut settings = options(&directory);
    tokio::fs::write(directory.path().join("not-directory"), b"occupied")
        .await
        .unwrap();
    settings.state_path = directory.path().join("not-directory/state.json");
    let child = service("touch \"$DEVD_TEST_DIR/started\"", directory.path());
    let manager = ServiceManager::new(config([("child", child)]), settings).unwrap();
    assert!(matches!(
        bounded(manager.run_until(std::future::pending())).await,
        Err(ServiceManagerError::StateIo { .. })
    ));
    assert!(!directory.path().join("started").exists());
}

#[tokio::test]
async fn test_orchestration_shutdown_is_reverse_dependency_order() {
    let directory = tempdir().unwrap();
    let script = "trap 'echo \"$NAME\" >> \"$DEVD_TEST_DIR/stopped\"; exit 0' TERM; echo ready; sleep 60 & wait";
    let mut root = service(script, directory.path());
    root.env.insert("NAME".into(), "root".into());
    let mut child = service(script, directory.path());
    child.env.insert("NAME".into(), "child".into());
    child
        .depends_on
        .push(dependency("root", DependencyCondition::Started));
    let mut run = RunningManager::start(
        ServiceManager::new(
            config([("root", root), ("child", child)]),
            options(&directory),
        )
        .unwrap(),
    );
    let mut ready = 0;
    bounded(async {
        while ready < 2 {
            let output = run.output.recv().await.unwrap();
            if output.bytes.windows(5).any(|s| s == b"ready") {
                ready += 1;
            }
        }
    })
    .await;
    run.shutdown().await.unwrap();
    assert_eq!(
        tokio::fs::read_to_string(directory.path().join("stopped"))
            .await
            .unwrap(),
        "child\nroot\n"
    );
}

#[tokio::test]
async fn test_orchestration_shutdown_cancels_dependency_wait_and_restart_delay() {
    let directory = tempdir().unwrap();
    let server = HttpServer::start(503).await;
    let mut root = sleeper(directory.path());
    root.healthcheck = Some(server.healthcheck());
    let mut dependent = sleeper(directory.path());
    dependent
        .depends_on
        .push(dependency("root", DependencyCondition::HttpReady));
    let mut crashing = service("exit 7", directory.path());
    crashing.restart.policy = RestartPolicyType::Always;
    crashing.restart.initial_delay = Duration::from_secs(60);
    let mut run = RunningManager::start(
        ServiceManager::new(
            config([
                ("root", root),
                ("dependent", dependent),
                ("crashing", crashing),
            ]),
            options(&directory),
        )
        .unwrap(),
    );
    run.until(|s| {
        s.services["root"].pid.is_some()
            && s.services["crashing"].status == ServiceState::Restarting
    })
    .await;
    let snapshot = run.shutdown().await.unwrap();
    assert!(snapshot
        .services
        .values()
        .all(|s| s.status == ServiceState::Stopped && s.pid.is_none()));
    assert_eq!(snapshot.services["crashing"].restart_count, 0);
}

#[tokio::test]
async fn test_orchestration_output_drains_both_pipes_without_subscribers() {
    let directory = tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/process-service.sh");
    let mut child = sleeper(directory.path());
    child.command = shell_words::join(["/bin/sh", fixture.to_str().unwrap(), "output"]);
    let state = bounded(
        ServiceManager::new(config([("child", child)]), options(&directory))
            .unwrap()
            .run_until(std::future::pending()),
    )
    .await
    .unwrap();
    assert_eq!(state.services["child"].last_exit_code, Some(0));
}

#[tokio::test]
async fn test_orchestration_output_preserves_raw_bytes_and_stream_identity() {
    let directory = tempdir().unwrap();
    let child = service(
        "printf 'stdout-data'; printf 'stderr-data' >&2; sleep 60 & wait",
        directory.path(),
    );
    let mut run = RunningManager::start(
        ServiceManager::new(config([("child", child)]), options(&directory)).unwrap(),
    );
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    bounded(async {
        while stdout.len() < 11 || stderr.len() < 11 {
            let output = run.output.recv().await.unwrap();
            assert_eq!(output.service, "child");
            assert_eq!(output.generation, 0);
            match output.stream {
                OutputStream::Stdout => stdout.extend(output.bytes),
                OutputStream::Stderr => stderr.extend(output.bytes),
            }
        }
    })
    .await;
    assert_eq!(stdout, b"stdout-data");
    assert_eq!(stderr, b"stderr-data");
    run.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_orchestration_persists_status_atomically_and_retains_final_snapshot() {
    let directory = tempdir().unwrap();
    let settings = options(&directory);
    let path = settings.state_path.clone();
    let mut run = RunningManager::start(
        ServiceManager::new(config([("child", sleeper(directory.path()))]), settings).unwrap(),
    );
    let running = run.until(|s| s.services["child"].pid.is_some()).await;
    bounded(async {
        loop {
            let saved: RuntimeSnapshot =
                serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
            if saved.services["child"].pid == running.services["child"].pid {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let final_state = run.shutdown().await.unwrap();
    let saved: RuntimeSnapshot =
        serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
    assert_eq!(saved, final_state);
    assert_eq!(saved.supervisor_pid, std::process::id());
    assert!(saved.services["child"].started_at.is_some());
}

#[tokio::test]
async fn test_orchestration_state_lock_rejects_duplicate_supervisor_and_releases() {
    let directory = tempdir().unwrap();
    let settings = options(&directory);
    let mut run = RunningManager::start(
        ServiceManager::new(
            config([("child", sleeper(directory.path()))]),
            settings.clone(),
        )
        .unwrap(),
    );
    run.until(|s| s.services["child"].pid.is_some()).await;
    let duplicate = ServiceManager::new(
        config([(
            "child",
            service("touch \"$DEVD_TEST_DIR/duplicate\"", directory.path()),
        )]),
        settings.clone(),
    )
    .unwrap();
    assert!(matches!(
        bounded(duplicate.run_until(std::future::pending())).await,
        Err(ServiceManagerError::StateIo { .. })
    ));
    assert!(!directory.path().join("duplicate").exists());
    run.shutdown().await.unwrap();
    bounded(
        ServiceManager::new(
            config([("child", service("exit 0", directory.path()))]),
            settings,
        )
        .unwrap()
        .run_until(std::future::pending()),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn test_orchestration_state_write_failure_cleans_running_children() {
    let directory = tempdir().unwrap();
    let settings = options(&directory);
    let path = settings.state_path.clone();
    let server = HttpServer::start(200).await;
    let mut child = sleeper(directory.path());
    child.healthcheck = Some(server.healthcheck());
    let mut run =
        RunningManager::start(ServiceManager::new(config([("child", child)]), settings).unwrap());
    let initial = run
        .until(|s| s.services["child"].status == ServiceState::Healthy)
        .await;
    bounded(async {
        loop {
            let saved: RuntimeSnapshot =
                serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
            if saved.services["child"].status == ServiceState::Healthy {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    tokio::fs::remove_file(&path).await.unwrap();
    tokio::fs::create_dir(&path).await.unwrap();
    server.status.store(503, Ordering::SeqCst);
    assert!(matches!(
        run.finish().await,
        Err(ServiceManagerError::StateIo { .. })
    ));
    reaped(initial.services["child"].pid.unwrap()).await;
}

#[tokio::test]
async fn test_orchestration_cancelling_run_kills_owned_process_groups() {
    let directory = tempdir().unwrap();
    let child = sleeper(directory.path());
    let mut run = RunningManager::start(
        ServiceManager::new(config([("child", child)]), options(&directory)).unwrap(),
    );
    let initial = run.until(|s| s.services["child"].pid.is_some()).await;
    let task = run.task.take().unwrap();
    task.abort();
    assert!(bounded(task).await.unwrap_err().is_cancelled());
    reaped(initial.services["child"].pid.unwrap()).await;
    bounded(
        ServiceManager::new(
            config([("child", service("exit 0", directory.path()))]),
            options(&directory),
        )
        .unwrap()
        .run_until(std::future::pending()),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_orchestration_shutdown_before_start_prevents_child_side_effects() {
    let directory = tempdir().unwrap();
    let child = service("touch \"$DEVD_TEST_DIR/started\"", directory.path());
    let snapshot = bounded(
        ServiceManager::new(config([("child", child)]), options(&directory))
            .unwrap()
            .run_until(async {}),
    )
    .await
    .unwrap();
    assert_eq!(snapshot.services["child"].status, ServiceState::Stopped);
    assert!(snapshot.services["child"].started_at.is_none());
    assert!(!directory.path().join("started").exists());
}

#[test]
fn test_orchestration_preflight_rejects_invalid_or_unsupported_configuration() {
    let directory = tempdir().unwrap();
    let settings = options(&directory);
    let mut child = sleeper(directory.path());
    child.restart.backoff = BackoffType::Exponential;
    assert!(matches!(
        ServiceManager::new(config([("child", child.clone())]), settings.clone()),
        Err(ServiceManagerError::UnsupportedBackoff { .. })
    ));
    child.restart.backoff = BackoffType::Fixed;
    child.restart.initial_delay = Duration::MAX;
    assert!(matches!(
        ServiceManager::new(config([("child", child.clone())]), settings.clone()),
        Err(ServiceManagerError::InvalidDuration { .. })
    ));
    child.restart.initial_delay = Duration::ZERO;
    child.healthcheck =
        Some(serde_yaml::from_str("type: socket\npath: /tmp/devd-test.sock").unwrap());
    assert!(matches!(
        ServiceManager::new(config([("child", child.clone())]), settings.clone()),
        Err(ServiceManagerError::Health { .. })
    ));
    child.healthcheck = None;
    let mut invalid = settings.clone();
    invalid.grace_period = Duration::MAX;
    assert!(matches!(
        ServiceManager::new(config([("child", child.clone())]), invalid),
        Err(ServiceManagerError::InvalidDuration { .. })
    ));
    let mut invalid = settings.clone();
    invalid.dependency_timeout = Duration::ZERO;
    assert!(matches!(
        ServiceManager::new(config([("child", child)]), invalid),
        Err(ServiceManagerError::ZeroDependencyTimeout)
    ));
    assert!(!settings.state_path.exists());
}

#[tokio::test]
#[ignore = "subprocess fixture for isolated Unix signal tests"]
async fn test_orchestration_signal_fixture() {
    let path = std::env::var_os("DEVD_SIGNAL_TEST_DIR").expect("fixture requires a directory");
    let directory = Path::new(&path);
    let child = service(
        "trap 'exit 0' TERM; touch \"$DEVD_TEST_DIR/ready\"; sleep 60 & wait",
        directory,
    );
    ServiceManager::new(
        config([("child", child)]),
        ManagerOptions::new(directory.join("services.json")),
    )
    .unwrap()
    .run()
    .await
    .unwrap();
}

#[tokio::test]
async fn test_orchestration_sigint_and_sigterm_trigger_graceful_shutdown() {
    for signal in [Signal::SIGINT, Signal::SIGTERM] {
        let directory = tempdir().unwrap();
        let mut supervisor = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "test_orchestration_signal_fixture",
                "--nocapture",
            ])
            .env("DEVD_SIGNAL_TEST_DIR", directory.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        bounded(async {
            while !directory.path().join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let initial: RuntimeSnapshot = serde_json::from_slice(
            &tokio::fs::read(directory.path().join("services.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        kill(Pid::from_raw(supervisor.id().unwrap() as i32), signal).unwrap();
        assert!(bounded(supervisor.wait()).await.unwrap().success());
        let saved: RuntimeSnapshot = serde_json::from_slice(
            &tokio::fs::read(directory.path().join("services.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(saved.services["child"].status, ServiceState::Stopped);
        reaped(initial.services["child"].pid.unwrap()).await;
    }
}
